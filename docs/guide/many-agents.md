English | [简体中文](many-agents.zh-CN.md)

# Many agents at once

When you run agents in several tabs, several chat views and several windows, the question is which of them needs you
right now. Eitri answers it with one set of rows: every Eitri process on your machine publishes one row per agent tab
(who it is, what state it is in, since when, whether it waits for you), and everything below reads those rows. Three
bundled pages show them, two keys walk you to the agents that wait, `eitri agents` prints them for your terminal and
your status bar, and a page you write yourself can read them through the `agents` capability.

Nothing here answers for you. A row shows that a card is waiting; the card is answered in the chat, with the keys of the
chat. Moving to a card never presses a key in it.

The layer is on by default. [Turn it off](#turn-it-off) with one line if you do not want it.

## The three roles

The pages have different jobs, can be on at the same time, and share one selection and one round (below), so what you
do in one shows in the others, in every window.

| role | page | where it sits | what it is for |
|---|---|---|---|
| glance | Strips, `agents-strips` | a sidebar at the left of the window | who needs you, always in view; rows never re-sort, so a row stays where you last saw it |
| act | The pass, `agents-pass` | one line above the modules | what needs you, one at a time: the agents that wait, oldest first, the round's place, next and back; it appears only while something waits or a round runs |
| manage | the table, `agents-top` | a module in the main area, hidden until you open it | everything: sort, filter, open a new agent, ask to stop one |

All three are plugins that ship with Eitri and are off until `init.lua` loads them
([Plugins](configuration.md#plugins)):

```lua
eitri.plugin.load("agents-strips")
eitri.plugin.load("agents-pass")
eitri.plugin.load("agents-top", { key = "g" })    -- prefix then g shows or hides the table
```

Strips has two settings: `narrow` (a narrow rail, drawn for a module about 62 pixels wide, with a glyph, what waits and the
tab number, instead of the full sidebar) and `show_idle` (`false` keeps idle rows in their place but draws only their glyph):

```lua
eitri.plugin.load("agents-strips", { narrow = true })
```

`narrow` changes what Strips draws, not how wide its column is: the column keeps its share of the window (a fifth).
Give the rail its width in pixels with an [open rule](configuration.md#where-a-new-module-opens) of your own; a width in
pixels stays the same however wide the window is:

```lua
eitri.layout.rule("lua:agents-strips", { at = "left", size = "62px" })
```

A rule places the column when the window places it, at a first launch; a layout you saved keeps the column as it was.
Closed and shown again, the rail comes back at its width. Drag its edge, or use `<prefix> Ctrl+Left`/`Ctrl+Right` with the keys on it, to
change the width in pixels (16 to 400).

In a narrow column the full sidebar keeps the label, the part that tells your agents apart: it drops the tool word
first, then the age. Hovering a row names it whole.

The pages run in the full Eitri window and in the page windows of [compositor windows](compositor-windows.md). They
do not run in the [companion window](companion.md) or on the Mac, where plugins are not loaded; a companion panel
still publishes its rows, so a page in another window and `eitri agents` in the terminal beside it show them.

With [compositor windows](compositor-windows.md) each page is a window of its own, which your compositor places and
sizes: Strips is an ordinary tiled window rather than a column at the left, and The pass does not come and go by
itself, because a page window only closes when you close it. Load The pass with a key there
(`eitri.plugin.load("agents-pass", { key = "P" })`) and open it when you want it; `<prefix> u` and `<prefix> B` work
without it.

## The shared grammar

Every page names a row by the agent tab's own key, so they agree on which row is selected.

- **One selection.** Moving the cursor in any page moves it in every page of every window that shows the row.
- **One round.** The round is a walk through the agents that wait, oldest first. Starting it, advancing it and leaving
  it show in all three pages, in whichever window you are in.

The keys inside a page:

| key | what it does |
|---|---|
| `j` / `k` | move the selection in the page's own order |
| `Enter` | on a row that needs you: start the round there, or move a running round there, and go to its chat with the card in front. On any other row: go to its chat |
| `c` | open a new agent tab in the selected row's project |
| `x` | ask to stop the selected agent (below) |
| `/` | filter: Strips dims the rows that do not match, the table hides them |
| `Esc` | move the keys to the editor. In The pass, while a round runs, `Esc` leaves the round instead and goes back to where it started |

Strips adds `m` to mark a row. The table adds `s` to cycle the sort (state, age, project, label), `S` to reverse it,
and it is the one page that re-sorts, because you open it, read it and close it, as you do `top`. It has no close key of
its own: its module key (`<prefix> g` above) hides it again.

- **Answering stays in the chat.** No page has a key that answers a card or a question or the trust question. When the
  round's current agent stops needing you (you answered in its chat), the round moves to the next one by itself, and
  the next chat's answer keys wait for a quarter of a second after it arrives, so a key you were still pressing
  cannot answer a card you have not read. The same wait applies whenever any page, `prefix u` or `prefix a` brings a
  chat forward.
- **Stopping is asked in the chat.** `x` brings the chat forward and it draws its own `Stop this turn? (y/n)` (also
  when Claude Code works on something the chat cannot show, which the row calls `running`), or
  `Stop the background work? (y/n)` when only background work runs. `y` stops; `n` or `Esc` does nothing. An agent with
  nothing running is refused (`idle`), in a companion panel too, and nothing comes forward.

## Keys without a page

| keys | what they do |
|---|---|
| `<prefix> u` | start the round at the agent that has waited longest, or go to the next one not yet visited; windows come forward as the round reaches them |
| `<prefix> B` | leave the round and go back to where it started |

With nothing waiting, `<prefix> u` says `no agent waits for you` in the status line for two seconds and ends a running
round. `<prefix> B` after the round has ended still goes back to where it started, until a round that has been idle
for ten minutes expires. Several [trust questions](permissions.md#trusting-a-project) of one window count as one stop,
because one answer settles them. Both keys work in a companion window too, and in the page windows of compositor
windows: a round started from a page there goes back to that page.

On GNOME the window that comes forward is raised by the key press that asked for it; on Hyprland, sway and niri the
window manager focuses it by process.

## Turn it off

```lua
eitri.config.set("agents.enabled", false)
```

With `agents.enabled` false in a window, that window publishes no rows, starts no reader and no control socket, and
`<prefix> u` and `<prefix> B` only say `agents are off in this window` in the status line; its rows leave the other
windows' pages at once. Its own pages say `agents are off in this window`. The key is read when the window starts.

## `eitri agents`: in your terminal and your status bar

```sh
eitri agents                 # a table: PROJECT WIN # AGENT STATE AGE LABEL
eitri agents --json          # one line: the whole view
eitri agents --watch         # the view, then one line per change
eitri agents --schema 1      # refuse to run unless the rows are this version
```

It reads the same rows as the pages and needs no window to answer, so a hung window cannot stall it. With no Eitri
window running the table says `no window shares agents` and the JSON has no rows; the exit code is 0. An unknown
argument or an unsupported schema exits 64, a read error 1. A closed pipe ends `--watch` quietly.

**tmux.** `eitri-waiting` is a small script, `~/.local/bin/eitri-waiting`:

```sh
#!/bin/sh
eitri agents --json | jq -r '[.rows[] | select(.class == "needs")] | length | if . > 0 then "⚑ \(.)" else "" end'
```

and `~/.tmux.conf` shows it:

```
set -g status-interval 5
set -g status-right '#(~/.local/bin/eitri-waiting) %H:%M'
```

**Waybar.** `--watch` writes a line whenever something changes; use that as the trigger and ask for the whole view each
time. `~/.local/bin/eitri-waybar`:

```sh
#!/bin/sh
show() {
  eitri agents --json | jq -c '[.rows[] | select(.class == "needs")] as $w | {
    text: (if ($w | length) > 0 then "⚑ \($w | length)" else "" end),
    class: (if ($w | length) > 0 then "waiting" else "quiet" end),
    tooltip: ($w | map("\(.project.name) \(.window.name)·\(.number) \(.label)") | join("\n"))
  }'
}
eitri agents --watch | while read -r _; do show; done
```

```json
"custom/agents": { "exec": "~/.local/bin/eitri-waybar", "return-type": "json" }
```

## Write your own page

A page that shows how many agents wait, below the editor. It is three files in
`~/.local/share/eitri/plugins/waiting/`.

`eitri-plugin.toml`:

```toml
id = "waiting"
title = "Waiting agents"
protocol = 1
site = "site"
entry = "index.html"
capabilities = ["agents"]

[module]
at = "below"
size = 0.08
focus = false
```

`site/index.html`:

```html
<!doctype html>
<meta charset="utf-8">
<button id="n">…</button>
<script src="main.js"></script>
```

`site/main.js`:

```js
const rows = new Map();
const el = document.getElementById("n");
const waiting = () => [...rows.values()].filter((r) => r.class === "needs");
const draw = () => {
  const w = waiting();
  el.textContent = w.length ? `⚑ ${w.length} waiting` : "no one waits";
};
el.onclick = () => {
  const w = waiting().sort((a, b) => a.waiting_since - b.waiting_since);
  if (w.length) eitriPlugin.call("agents.round", { op: "start", key: w[0].key });
};
const load = (view) => { rows.clear(); view.rows.forEach((r) => rows.set(r.id, r)); draw(); };
eitriPlugin.on("agents.row", (r) => { rows.set(r.id, r); draw(); });
eitriPlugin.on("agents.gone", (g) => { rows.delete(g.id); draw(); });
eitriPlugin.on("agents.reset", load);
eitriPlugin.call("host.hello", {})
  .then(() => eitriPlugin.call("agents.watch"))
  .then(load)
  .catch((e) => { el.textContent = e.code === "disabled" ? "agents are off" : String(e); });
```

and in `init.lua`: `eitri.plugin.load("waiting")`.

The click is the gesture the call needs. It starts the round at the agent that has waited longest (or moves a round
already running there), and that agent's chat comes forward with its card in front, as `Enter` on a waiting row does in
the bundled pages. `agents.focus` goes to a row's chat without the round. The host answers `agents.watch` before it
sends this document any event, so `load` never overwrites a newer row.

The calls a page with the `agents` capability can make:

| call | arguments | what it does |
|---|---|---|
| `agents.watch` | none | answers the whole view; the page then hears the events below |
| `agents.focus` | `{id, card_first?}` | goes to that row's chat, with its card in front when `card_first` is true |
| `agents.new` | `{project}` | opens a new agent tab in that project (a project key from the view's `projects`) |
| `agents.select` | `{key}` | moves the shared selection |
| `agents.round` | `{op, key?}` | `op` is `start`, `next` or `leave`; `start` with the `key` of a row that waits starts the round there (or moves a running round there) and goes to its chat with the card in front |
| `agents.stop` | `{id}` | goes to that row's chat and asks `Stop this turn? (y/n)` there |

The events are `agents.row` (a row changed), `agents.gone`, `agents.reset` (the whole view, after changes the page
could not hear), `agents.selection` and `agents.round`. A page that is not on screen hears nothing and gets one
`agents.reset` when it comes back.

A page that declares `agents` also gets `window.eitriAgents` before its own script runs: a row store keyed by row key
that applies those events, a glyph and a word for every state (with a fallback by class for a state a newer Eitri
added), and the keys of [the shared grammar](#the-shared-grammar) declared through the keys kit, so your page fits
beside the bundled three. It also gets `window.eitriKeys`. [Pages you build](pages.md) covers the rest of the plugin
format.

### The rows

The whole view and each line of `eitri agents --watch` carry `"v": 1`; a row itself does not (the example below has no
`v`), and neither does the payload of a page event. A reader must ignore fields it does not know and map a state it
does not know by its `class`. One row:

```json
{
  "id": "a3f09c1d2e4b5f60.7",
  "key": "k-5c1e0b9a7d3f2e18",
  "agent": "claude",
  "project": { "key": "9b2c4e7a10f3d856", "name": "eitri" },
  "window": { "instance": "a3f09c1d2e4b5f60", "name": "1", "focused": true },
  "number": 3,
  "label": "close question",
  "label_from": "title",
  "shown": false,
  "state": "card",
  "class": "needs",
  "since": 1793865600123,
  "needs": { "cards": 1, "questions": 0, "trust": false },
  "waiting_since": 1793865600123,
  "unread": false,
  "tool": "Bash",
  "background": { "calls": 0, "shells": 0, "monitors": 0, "agents": 0, "other": 0 },
  "queued": 0,
  "mode": "manual",
  "ended_how": null,
  "moving": false
}
```

| field | meaning |
|---|---|
| `id` | `<instance>.<tab>`; valid while the row exists; the argument of `focus` and `stop` |
| `key` | `k-` and 16 hex digits, the tab's own, kept when the tab moves to another window or is resumed |
| `agent` | `claude`, `codex` or `acp:<name>` |
| `project` | `key` (16 hex digits, no path) and `name` (the folder's name, cut to 40 characters) |
| `window` | `instance` (random per process start), `name` (`1`, `2`, `u` for an unpaired window, `c1`..`c9` for compositor chat windows, `companion`), `focused` |
| `number` | the number on the tab strip of that window (not global) |
| `label`, `label_from` | at most 40 characters; from `rename`, `title`, `session` or `new` |
| `shown` | the tab is the active tab of a chat that is on screen |
| `state`, `class` | below |
| `since` | when the row entered its state, in milliseconds since the epoch of the writing process's clock |
| `waiting_since` | when the oldest card, question or trust question was first seen, or `null`; the round orders by it |
| `needs` | counts of cards and questions, and whether a trust question waits (a tab can hold a card and a question) |
| `unread` | a turn finished while the chat was not on screen |
| `tool` | the open tool call's name (or the oldest card's), never its input |
| `background` | `calls` is the total; the four counts by kind can all be 0 while `calls` is not |
| `queued` | messages typed and waiting for the turn to end |
| `mode` | `manual`, `auto` or `bypass` |
| `ended_how` | `closed`, `stopped`, `unavailable`, `start_failed` or `tripped`; never a reason text |
| `moving` | the tab is in the middle of moving to another window |

The states, in the order that decides which one a row has (the first that holds wins):

| state | class | glyph | when |
|---|---|---|---|
| `card` | needs | `⚑` | a permission card or a tool card waits |
| `question` | needs | `?` | a question set waits |
| `trust` | needs | `?` | a start waits for the trust answer |
| `failed` | ended | `✕` | the tab could not start, or its session died unasked |
| `ended` | ended | `✕` | the session closed or you stopped it, and the tab is still open |
| `running` | working | `●` | a turn runs |
| `background` | working | `◌` | no turn runs, background work does |
| `starting` | working | `…` | the session is being opened |
| `done` | quiet | `•` | idle, and a turn finished while the chat was not on screen |
| `idle` | quiet | `·` | a live session with nothing running and nothing unread |
| `new` | quiet | (blank) | a tab with no session yet |

The whole view, which is what `eitri agents --json` prints and `agents.watch` answers:

```json
{ "v": 1, "at": 1793865600200, "enabled": true, "projects": [ ... ], "rows": [ ... ],
  "selection": { "key": "k-…", "by": "a3f09c1d2e4b5f60", "at": 1793865600100 } | null,
  "round": { "from": { "instance": "a3f09c1d2e4b5f60", "module": "…", "tab": null }, "current": "k-…",
             "visited": [ ... ], "started_at": 1793865500000, "at": 1793865600100 } | null }
```

`projects` lists every project that has a running window, `{key, name}`, including one whose windows hold no tab, so a
page can offer `new` there. Rows come ordered by project name and key, window name and instance, and number. The lines
of `--watch` wrap each change in an envelope with `v` and `type`:

```json
{ "v": 1, "type": "row",  "row": { ... } }
{ "v": 1, "type": "gone", "id": "a3f09c1d2e4b5f60.7", "key": "k-5c1e0b9a7d3f2e18", "why": "closed" }
{ "v": 1, "type": "reset", "view": { ... } }
{ "v": 1, "type": "selection", "selection": { ... } | null }
{ "v": 1, "type": "round", "round": { ... } | null }
```

A page does not get these envelopes. Its callback receives the payload alone, whose names are the ones inside the
envelope: for `agents.row` the row itself (`event.id`, not `event.row.id`), for `agents.gone` `{id, key, why}`, for
`agents.reset` the whole view (this one has `v`), for `agents.selection` the selection or `null`, for `agents.round`
the round or `null`.

`why` is `closed` (the tab), `moved` or `window` (its process ended, or turned the layer off). A process publishes at
most 64 rows and 64 KiB; past that it publishes the first 64 by tab number.

## Limits

- **A row never carries conversation text.** No prompt, no reply, no tool input, no path, no failure reason. The label
  is the one piece derived from a prompt (the title the chat gives the tab, cut to 40 characters), and it travels to
  every page and to any process of your user that reads the row files. Eitri's own records and the agents' transcripts
  hold far more for the same user, so the files add nothing such a process could not already read.
- **`agents` and `net` cannot be declared together.** A page that can read your agents cannot also reach the network;
  load refuses the pair with `agents and net cannot be declared together`.
- **Every call but `agents.watch` needs a gesture.** The page's module must have the keys, and a key, a button or a
  touch must have happened in the page less than a second before the call. A page's own script cannot fake one.
  `agents.new` is also refused within a second of the previous one.
- **Nothing answers for you.** No row, call or event answers a card, a question or a trust question.
- **Page windows beside your own Neovim** load plugins with five capabilities only, so `agents` is refused there.
- **Only running windows are listed.** A project with no window open is not in the view, and `agents.new` for it
  answers `no_window`; starting one is not something these calls do.
- **`eitri agents` only reads.** It cannot take you to an agent: raising a window on GNOME needs the key press or
  click that asked for it, which a script does not have.
- **Two Eitri versions at once** (a window left open across an upgrade) both show in the newer pages. The older window
  ignores a newer file with one log line.
- **When the runtime directory is unusable** (`XDG_RUNTIME_DIR` unset and no usable temporary directory, or a path
  too long for a socket) the process runs without the layer and logs one line; nothing else changes.
- **Close and rename are not calls, and stop only asks.** `agents.stop` takes you to the row's chat and asks its
  question there; nothing stops until you answer `y` in the chat. Closing and renaming stay in the chat. Interrupting
  without stopping, and answering, are not offered.
