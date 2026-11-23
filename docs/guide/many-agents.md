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
| glance | Strips, `agents-strips` | a sidebar at the left of the window, hidden until you open it with `<prefix> U` | who needs you, one key away; rows never re-sort, so a row stays where you last saw it |
| act | The pass, `agents-pass` | one line above the editor | what needs you, one at a time: the agents that wait, oldest first, the round's place, next and back; it appears only while something waits or a round runs |
| manage | the table, `agents-top` | a module in the main area, hidden until you open it | everything: sort, filter, open a new agent, ask to stop one |

Each takes the keys as any module does: from the editor, `<prefix> Left` reaches Strips once it is open and `<prefix> Up`
The pass, `<prefix> o` goes round every module, and a page's own key opens it with the keys and, pressed again with the
keys on it, hides it. Strips comes with its own key, `<prefix> U` (beside `u` and `B`, the round's keys below); the
table has one only if you give it one, as below.

All three are plugins that ship with Eitri and are off until `init.lua` loads them
([Plugins](configuration.md#plugins)); to try one in the window you are in, `Enter` on it under the empty tab's Pages, or
`:Eitri plugin load agents-top` (for this run only):

```lua
eitri.plugin.load("agents-strips")                -- prefix then U shows or hides the sidebar
eitri.plugin.load("agents-pass")
eitri.plugin.load("agents-top", { key = "g" })    -- prefix then g shows or hides the table
```

`eitri.plugin.load("agents-strips", { key = "y" })` gives Strips another key instead, and `{ key = false }` none. If
something already uses `U` (a binding in your `init.lua` or your tmux configuration, or another page you gave
`key = "U"`), that keeps it: Strips loads with no key, and the terminal you started Eitri from says why. Once you have
opened Strips, a layout you saved keeps it open at the next start; a layout saved by an Eitri that showed Strips at
start keeps it shown too, and `<prefix> U` hides it from there.

Strips has two settings: `narrow` (a narrow rail, drawn for a module about 62 pixels wide, with a glyph, what waits and the
tab number, instead of the full sidebar) and `show_idle` (`false` keeps idle rows in their place but draws only their glyph):

```lua
eitri.plugin.load("agents-strips", { narrow = true })
```

`narrow` changes what Strips draws, not how wide its column is: the column keeps its share of the window (a fifth).
Give the rail its width in pixels with an [open rule](configuration.md#where-a-new-module-opens) of your own; a width in
pixels stays the same however wide the window is:

```lua
eitri.layout.rule("lua:agents-strips", { at = "left-edge", size = "62px" })
```

A rule places the column when it first opens; a layout you saved keeps the column as it was.
Closed and shown again, the rail comes back at its width. Drag its edge, or use `<prefix> Ctrl+Left`/`Ctrl+Right` with the keys on it, to
change the width in pixels (16 to 400). Beside the pixel width the column keeps a share of the window, at first the
smallest there is (a twentieth), growing and shrinking as you drag the width; that share is what you see only where the width does not apply: in a layout an older Eitri
reads, or with the rail moved into a column of modules stacked one above the other.

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
  a card in the next chat is answered only in card mode, which you enter yourself (`Alt-n`), so a key you were
  still pressing cannot answer a card you have not read. The same wait applies whenever any page, `prefix u` or `prefix a` brings a
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

`WIN` is the window's name within its project (`1`, `2`, ...; the names are below), so the first window of every
project reads `1`: `PROJECT` and `WIN` together name a window.

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

**A line for each new wait.** The next two need to know when an agent starts waiting, not how many wait. `eitri-on-wait`
prints one line each time that happens, naming the agent's project, window, tab and state (`eitri 1·2 card`). Agents
already waiting when it starts are not printed; one that stops waiting and waits again is. `~/.local/bin/eitri-on-wait`:

```sh
#!/bin/sh
# One line each time an agent starts waiting for you: "<project> <window>·<tab> <state>".
# Agents already waiting when it starts are not printed; an agent that stops waiting and waits again is.
# A tab that moves to another window keeps its key and gets a new id, so a wait is kept by key with the id it has,
# and a departure counts only for the id that left.
eitri agents --watch | jq -n --unbuffered -r '
  def slot: if (.key // "") == "" then .id else .key end;
  def line: "\(.project.name) \(.window.name)·\(.number) \(.state)";
  def waits($rows): [$rows[] | select(.class == "needs") | {key: slot, value: {id, line: line}}] | from_entries;
  foreach inputs as $e ({waiting: {}, say: []};
    if $e.type == null then {waiting: waits($e.rows), say: []}
    elif $e.type == "reset" then .waiting as $old | waits($e.view.rows) as $now
      | {waiting: $now, say: [$now | to_entries[] | select(.key as $k | $old | has($k) | not) | .value.line]}
    elif $e.type == "row" then ($e.row | slot) as $k
      | if $e.row.class != "needs" then {waiting: (.waiting | del(.[$k])), say: []}
        elif .waiting[$k] then {waiting: (.waiting + {($k): {id: $e.row.id, line: ($e.row | line)}}), say: []}
        else ($e.row | line) as $l | {waiting: (.waiting + {($k): {id: $e.row.id, line: $l}}), say: [$l]} end
    elif $e.type == "gone" then ($e | slot) as $k
      | if .waiting[$k].id == $e.id then {waiting: (.waiting | del(.[$k])), say: []} else .say = [] end
    else .say = [] end;
    .say[])'
```

**Desktop notifications.** `~/.local/bin/eitri-notify`, started from your desktop's autostart or a `systemd --user`
service:

```sh
#!/bin/sh
# A desktop notification each time an agent starts waiting.
eitri-on-wait | while IFS= read -r line; do
  notify-send --app-name=Eitri "An agent waits for you" "$line"
done
```

**Your phone.** [ntfy](https://ntfy.sh) pushes a message posted to a topic on your server to the ntfy app on your
phone. `~/.local/bin/eitri-ntfy`:

```sh
#!/bin/sh
# A push to your phone through your own ntfy server each time an agent starts waiting.
# NTFY_URL is the topic's address on your server, for example https://ntfy.example.com/my-agents.
: "${NTFY_URL:?set NTFY_URL to your ntfy topic address}"
eitri-on-wait | while IFS= read -r line; do
  curl -fsS -H "Title: An agent waits for you" -d "$line" "$NTFY_URL" > /dev/null
done
```

The line it posts holds the project's folder name, which then leaves this machine for your server; the label, which
comes from your prompt, stays out of it. Run it as `NTFY_URL=https://ntfy.example.com/my-agents eitri-ntfy`, with
your server's address and a topic name nobody else would guess.

The five scripts on this page are in Eitri's source tree under `examples/agents/`, with recorded sample rows in `examples/agents/fixtures/`.

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
beside the bundled three. It also gets `window.eitriKeys`. [Write a plugin](write-a-plugin.md) is the whole reference
for plugin authors, and [Pages you build](pages.md) the short introduction.

### The rows

Every field of a row, the states and what decides them, the whole view and the lines of `--watch` are in
[Write a plugin](write-a-plugin.md#the-rows), with each call's arguments, results and refusals.

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
