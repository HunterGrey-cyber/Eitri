English | [简体中文](pages.zh-CN.md)

# Pages you build

Eitri can show web pages of your own as modules of the window, beside the editor, the chats and the terminals.
On Hyprland, sway and niri each page is a window of its own by default, which your window manager arranges
([Compositor windows](compositor-windows.md)). There are two kinds:

- **A Lua panel** is one line of `init.lua` that shows an HTML file or an address, such as your development
  server. It is an ordinary web view and knows nothing of Eitri.
- **A plugin** is a directory with an `eitri-plugin.toml` and a small web site. Its page gets Eitri's colours,
  Eitri's page keys and a bridge to a closed list of host powers that the manifest names: saving its own data,
  moving the keys to the editor, running the plugin's own programs, following the buffer you edit. The
  [Markdown preview](markdown-preview.md) that ships with Eitri is one.

**Both are experimental.** The manifest, the bridge and the keys kit may change during 0.x, and a release note will
say when they do. Neither is available in the companion window.

## Trust

There is no consent screen yet. **Loading a plugin is the grant**: an `eitri.plugin.load` line in your own `init.lua`
gives the plugin every power its manifest lists, and a plugin with tasks runs its programs as you, with your files
and your network. Treat a plugin as you treat any program you run: load only one you wrote or have read. Eitri
records each plugin's list of powers under `~/.local/state/eitri/plugins/<id>/` and logs a change at the next start;
it asks nothing.

A Lua panel runs whatever page you point it at, with the network access any web page has. Point it only at pages you
trust.

## A Lua panel

Put an HTML file in `<config>/panels/` and register it in `init.lua`. `<config>` is the directory `init.lua` is
read from: `$EITRI_CONFIG_DIR`, else `$XDG_CONFIG_HOME/eitri` when that is an absolute path, else `~/.config/eitri`.

```lua
eitri.panel.register{
  id = "status",                                  -- the module is lua:status
  title = "Status",                               -- the name on its chip and in the tray
  position = "side",                              -- "side", "bottom" or "main", where it goes the first time
  key = "h",                                      -- optional: <prefix> h shows it and gives it the keys
  content = { type = "webview", url = "status.html" },
}
```

```html
<!doctype html>
<meta charset="utf-8">
<title>Status</title>
<h1>Today</h1>
<ul>
  <li>Review the parser change</li>
  <li>Write the release notes</li>
</ul>
```

- `url` is a file name under `panels/`, or used exactly as given when it starts with `/` or has a scheme
  (`file://`, `https://`, `http://localhost:5173` for a development server, `data:`).
- `position` is a first-launch hint only: after that the window keeps where you put the panel, and an
  [open rule](configuration.md#where-a-new-module-opens) for `lua:status` decides over it.
- `key` must be one character that nothing after the prefix uses: not a default key, not one stock tmux binds after
  its prefix, not `v`, and not `E`, `A`, `T` or `V`. `b`, `g`, `h`, `j`, `k`, `u` and `y` are free by default. A
  clash leaves the page without a key and is listed when Eitri starts, naming the key and what holds it.
- The id must not be `terminal#2` to `terminal#9` or `agent#2` to `agent#9`; registering the same id again replaces
  the earlier panel.
- The page is a plain web view: its scripts run, its links open inside it, and it can reach the network. It gets no
  bridge to Eitri, no colours and no keys kit, so a page that needs those should be a plugin. Eitri's own keys (the
  prefix, `Ctrl+h/j/k/l`) still work there, so `<prefix> e` takes you back to the editor.

## A plugin

A plugin is a directory. Put it in `~/.local/share/eitri/plugins/<id>/` and load it by name, or anywhere and load it
by path ([how a name is found](configuration.md#plugins)):

```lua
eitri.plugin.load("scratch", { key = "y" })
```

`eitri.plugin.load` is taken only while `init.lua` runs. Its options are the plugin's own settings, `key` (a module
key, as for a Lua panel) and `position`. Each load prints the directory it used in the terminal you started Eitri
from. A mistake in the manifest means the page is not loaded, and Eitri lists it when it starts, naming the key at fault.

### A complete example

A note that stays between launches. Four files:

```text
~/.local/share/eitri/plugins/scratch/
├── eitri-plugin.toml
└── site/
    ├── index.html
    ├── page.js
    └── style.css
```

`eitri-plugin.toml`:

```toml
id = "scratch"
title = "Scratch"
protocol = 1
site = "site"
entry = "index.html"
capabilities = ["state", "layout.focus"]
```

`site/index.html`:

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Scratch</title>
<link rel="stylesheet" href="style.css">
<script src="/.eitri/keys.js"></script>
<script src="page.js" defer></script>
</head>
<body>
<textarea id="note" aria-label="Note"></textarea>
</body>
</html>
```

`site/style.css`:

```css
body { margin: 0; background: var(--nv-bg); color: var(--nv-fg); font-family: var(--nv-font-prose); }
textarea {
  box-sizing: border-box; width: 100%; height: 100vh; padding: 12px; border: 0;
  background: var(--nv-surface); color: var(--nv-surface-fg); font-family: var(--nv-font-mono);
}
textarea:focus { outline: 2px solid var(--nv-focus-ring); }
```

`site/page.js`:

```js
const host = window.eitriPlugin;
const note = document.getElementById("note");

async function start() {
  const hello = await host.call("host.hello");
  const saved = await host.call("state.read", { key: "note" });
  note.value = saved.value ?? "";

  let timer = 0;
  note.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(() => host.call("state.write", { key: "note", value: note.value }), 300);
  });

  await window.eitriKeys.install({
    rows: [
      { action: "note.edit", keys: ["i"], context: "note", desc: "type in the note" },
      { action: "note.back", keys: ["q"], context: "note", desc: "back to the editor" },
    ],
    run(action) {
      if (action === "note.edit") { note.focus(); return true; }
      if (action === "note.back") { host.call("layout.focus", { target: "editor" }); return true; }
      return false;
    },
    initial: { timeoutlen: hello.timeoutlen, hasKeys: true, prefix: hello.prefix, settings: hello.keys_settings },
  });
}

start();
```

Restart Eitri. `<prefix> y` shows the page and gives it the keys (in a project with no saved layout yet it is already
open, to the right). `i` types in the note and `Esc` stops typing, `q` gives the keys back to the editor, `?` lists
the page's keys, and `j`, `k`, `/` and `f` work as in every plugin page. The note is kept in
`~/.local/state/eitri/plugins/scratch/state/`.

### The manifest

| key | what it holds |
|---|---|
| `id` | lower-case letters, digits and `-`, at most 40 characters, no `-` at either end; the name you load it by |
| `title` | the name on the module's chip |
| `protocol` | `1` |
| `site`, `entry` | the directory of the page's files, relative to the plugin, and the page inside it |
| `capabilities` | the host powers the page may use, from the list below; a verb of any other is refused with `not_declared` |
| `[settings.<name>]` | an option of `eitri.plugin.load`: `kind = "bool"`, `"choice"` (with `values`, one to eight words), `"int"` (a whole number, with `min` and `max`; its `default` may be left out, and the option is then unset until `init.lua` gives it a value) or `"dir"` (a path, created if missing), and a `default` |
| `[module]` | `start` (`"shown"` or `"hidden"` at a first launch), `at` (an [open rule](configuration.md#where-a-new-module-opens) place), `size`, `focus` |
| `[roots]` | the directories `editor.open`, `editor.assist` and tasks may touch, as `${plugin}` or a `dir` setting |
| `[tasks.<name>]` | `argv` (the first word `${plugin}/<path>`, a program inside the plugin), `cwd` (inside a root) and `timeout_secs` (`0` for none) |
| `[vars.<name>]` | a value the page passes to a task: `chars` (`"lower-digit-dash"`, `"lower-digit"` or `"digit"`) and `max_len` (1 to 256) |
| `[net]` | `origins` (`https://host[:port]` the page may fetch from) and `images` (`"https"` or `"off"`, or a choice setting with those two values) |
| `[follow]` | `filetypes` the page follows in the editor, and `auto_show` (a bool setting) |
| `[commands.<Name>]` | a Neovim command that shows, toggles or hides the page (`does = "show"`, `"toggle"` or `"hide"`) |
| `[page]` | `handoff = true` when the page hands its state to the page that replaces it in another window |

Every table refuses a key it does not know, naming it. Tasks run without a shell: each argument is one word, with
`${plugin}`, a setting or a declared variable inside it, and the program's output comes back to the page line by
line.

The capabilities:

| capability | what the page may then do |
|---|---|
| `state` | keep JSON values by key (`state.read`, `state.write`) and append to logs (`log.append`, `log.read`) under `~/.local/state/eitri/plugins/<id>/` |
| `layout.focus` | give the keys to the editor or take them (`layout.focus` with `target` `"editor"` or `"self"`) |
| `editor.open` | open a file inside a root in the editor, at a line and column |
| `editor.assist` | turn the editor's diagnostics and inlay hints on or off for files under a root |
| `tasks` | start and cancel the plugin's own `[tasks]` (`task.start`, `task.cancel`; output arrives as `task.output` and `task.exit` events) |
| `net` | fetch from the `[net]` origins; nothing else leaves the page |
| `editor.follow` | follow the buffer you edit (`editor.view`, `editor.text` events), scroll it and move its cursor |
| `editor.commands` | define the `[commands]` in Neovim |
| `agents` | see every agent tab of every Eitri window (`agents.watch`; `agents.row`, `agents.gone`, `agents.reset`, `agents.selection` and `agents.round` events while the page is on screen), and bring one forward, open a new one, move the shared selection, walk the agents that wait (`agents.round`) or ask one to stop in its chat (`agents.focus`, `agents.new`, `agents.select`, `agents.round`, `agents.stop`); each of these needs a key press or click in the page in the last second, with the page holding the keys (`no_gesture` otherwise), and `agents.new` works at most once a second. Nothing a page does answers a permission card or a question: that stays in the chat. Cannot be declared with `net` |

### What the page runs in

- The page is served from `eitri-plugin://<id>/`, only the files under `site` (HTML, CSS, JavaScript, JSON, images
  and fonts), with a strict content security policy: scripts only from the site, no inline script and no `eval`; style
  sheets from the site, plus inline styles (`<style>` elements and `style` attributes); images from the site or
  `data:`, and from any `https` address when `[net]` `images` is (or its setting resolves to) `"https"`; and network
  requests only to the `[net]` origins. An `https`, `http` or `mailto` link outside the site opens outside Eitri when
  you press it; a script cannot open one.
- **The bridge** is `window.eitriPlugin`: `call(verb, args)` returns a promise of the result, or rejects with an
  error whose `code` says why (`not_declared`, `bad_request`, `outside_roots`, `too_large` and a few more), and
  `on(event, fn)` listens. Call `host.hello` first: its result holds the page's settings, the theme, the window's
  prefix and `timeoutlen`, and the runs of its tasks that are still going. A message may be at most 256 KiB, a stored
  value 192 KiB, and a plugin keeps at most 64 keys.
- **Colours and fonts** arrive as `--nv-*` CSS variables on the page's root (`--nv-bg`, `--nv-fg`, `--nv-surface`,
  `--nv-border`, `--nv-link`, `--nv-font-mono`, `--nv-font-prose`, the `--nv-syn-*` syntax colours and more), and
  change live with your colourscheme. `prefers-color-scheme` follows Eitri's light or dark. Every Eitri window except one beside
  your own Neovim also sends `--nv-text-size`, the editor's text size in the page's pixels (for example `18.67px`), which moves
  with `guifont`; a page that wants its text the editor's size reads `var(--nv-text-size, 14px)`. A window beside your
  own Neovim sends none.
- **The keys kit** at `/.eitri/keys.js` gives the page the keys every plugin page shares: `j`/`k` and `d`/`u`
  scroll, `gg`/`G`, `/` finds, `f` labels what can be pressed, `?` lists the keys, `H`/`L` go back and forward, and
  `v`/`V` select text as Vim's visual mode does (`y` copies it). Pass `visual: {start, follow, keys}` to `install` to
  start a selection at your page's own cursor, follow it as it moves, and add keys of your own for it.
  `window.eitriKeys.install({rows, run, initial})` adds your rows; `run(action)` returns `true` when it handled the
  key. While a text field has the keys every key goes to it except `Esc`. You can change a page's keys in `init.lua`
  with `eitri.keymap.set("plugin:<id>", "<key>", "<action>")`.
- **Eitri's keys win**: the prefix and the key after it, `Ctrl+h/j/k/l`, `Ctrl+=`, `Ctrl+-`, `Ctrl+0` and `F11` never
  reach the page, and a row may not use them, a `Ctrl+Shift` chord or a `Super` chord. A page that handles a key
  itself, without the kit, and moves the keys to the editor should call `preventDefault()`, or the key reaches the
  editor too.
- The text size follows the window's (`Ctrl+=`), and `<prefix> Ctrl+=` sizes the page alone.

## A brief for your own agent

To have an agent write a plugin for you, give it this page and a brief such as:

```text
Write an Eitri plugin: a directory with eitri-plugin.toml (id, title, protocol = 1, site, entry,
capabilities) and a static site under `site`. Follow docs/guide/pages.md in the Eitri repository.

- The page runs in WebKitGTK at eitri-plugin://<id>/ under a strict CSP: no inline script, no eval,
  scripts and styles only from the site, network only to origins listed under [net].
- It reaches Eitri only through window.eitriPlugin.call(verb, args) and .on(event, fn); call
  host.hello first. Declare only the capabilities the page uses.
- Load /.eitri/keys.js and call window.eitriKeys.install({rows, run, initial}) after host.hello, so
  every action has a key. Never bind the prefix, Ctrl+h/j/k/l, Ctrl+= / Ctrl+- / Ctrl+0, F11, or a
  Ctrl+Shift or Super chord.
- Use the --nv-* CSS variables for every colour and font.
- Tasks run only programs inside the plugin directory, without a shell.

What the page should do: <describe it here>.
```

Then read what it wrote before you load it: loading it trusts it.
