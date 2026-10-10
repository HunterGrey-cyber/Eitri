English | [简体中文](agents.zh-CN.md)

# Codex and other agents

Every tab in the agent panel runs one agent, chosen when the tab is made. Claude Code is the default. A tab can
also run Codex, or any agent you register that speaks the Agent Client Protocol (ACP) over standard input and
output. This page covers what you need for those two, how to open such a tab, what Eitri asks you before it
starts one, who answers an agent's requests, and what is weaker or missing compared with a Claude tab.

Eitri runs the agents you installed and signed in to; it ships none of them. The panel, the keys, the queue,
[turn review](turn-review.md) and the rest of [Getting started](getting-started.md) work the same in every
tab. The pages on [permissions](permissions.md) and the [Claude account](configuration.md#accounts) describe
Claude tabs only, except where this page says otherwise.

## Opening a tab for another agent

- **`:tabnew codex`**, or `:tabnew` followed by `claude` or the name of an agent you registered, in the panel's command line
  (`:` in browsing, or `<prefix> :` from anywhere; see [the command line](keys.md#the-command-line)). It opens
  a new tab that runs that agent and puts you in it. It also works on a fresh window's start screen. `:tabnew`
  with no word opens a tab of the default agent, the same tab `<prefix> c` opens. A word that names no agent
  is refused with a message that lists the words that are accepted.
- **`agent.default`** in `init.lua` chooses the agent a new tab and a fresh window's first tab run:

  ```lua
  eitri.config.set("agent.default", "codex")    -- "claude" (the default), "codex" or a registered name
  ```

  Anything else stops Eitri at startup with a message naming the key.

A tab that runs another agent names it in the tab bar, and its empty tab's menu says `New Codex session` (or
`New` and the agent's name), with the agent's name in place of the Claude account on the line above it. Nothing
starts until you send the first message and the project has been asked about (below). `<prefix> i` shows the tab's details; for these tabs it has its own rows,
including `runs without asking` and `mode lag`.

## Codex

### What you need

- The `codex` command, installed, and signed in with `codex login`. Without a login the tab says "Codex is not
  signed in: run `codex login` in a terminal".
- Codex 0.160.0 or newer. Eitri has been tried with 0.160.0 to 0.161.0; the tab's `agent` row says so. A newer
  Codex still runs, and the row then reads "untested Codex" with its version and the newest one tried. An older one is
  refused with "update Codex".

Eitri picks the program in this order: the `EITRI_CODEX` environment variable (an absolute path to a file),
then `agent.codex.command` in `init.lua` (an absolute path, or a bare command name looked up on `PATH`), then
`codex` on `PATH`. An `EITRI_CODEX` that is not an absolute path to an existing file stops Eitri at startup and names the
variable, and so does an `agent.codex.command` that is a relative path with a directory part. A `codex` that
cannot be found otherwise fails only the Codex tab's start: with nothing on `PATH` the message is "codex not
found: install it and run `codex login`, or set EITRI_CODEX", and an `agent.codex.command` that names no file
says so by name. `PATH` entries that are empty or relative are skipped, because the
agent runs in your project directory and such an entry would name a file the project supplied.

### Trust: the question Codex tabs ask

Like a Claude tab, a Codex tab asks before the first session in a project whether to load the project's own
configuration, and shows what it found. A project with nothing of the kind to trust starts at once, with no
question. The question reads "Trust this project's Codex configuration?". For
Codex Eitri looks, in the project directory and in each directory above it up to the top of its git repository
(never above your home directory), at:

- `.codex/`: its configuration, rules, skills and hooks;
- `AGENTS.md` and `AGENTS.override.md`, and any other instruction file names your own Codex `config.toml` lists
  as alternatives;
- `.agents/skills/` and `.agents/plugins/`, and at the top level the plugin listings other tools keep.

A linked git worktree also shows the main checkout's `.codex/`. Your own Codex directory (`CODEX_HOME`, or
`~/.codex`) is not part of the project and is named as not read.

The answer works as it does for Claude (see [Trusting a project](permissions.md#trusting-a-project)): `y` is
remembered for the project and tied to what you saw, so a change asks again; a file Eitri cannot check makes
the answer last one start; nothing in the question is cut, so a project whose question would pass 2 MiB gets
a short question that takes only `n` (or `Esc`) and `y` is refused; `:trust` and `:untrust` act on the agent of the tab on screen. Each agent has its
own question, its own remembered answer and its own `:untrust`: trusting a project for Claude says nothing
about Codex, and two waiting tabs of two agents are asked twice.

What `n` does is the part to know. A Codex tab can be started without the project's configuration, so `n`
starts it that way: Eitri tells Codex not to trust the project directory (or any directory above it), and
skills, yours included, are switched off while the project is untrusted. Your own Codex configuration always loads and cannot be turned off from
Eitri. If Codex still reports that it loaded something from the project in such a session (an instruction file
from the project, a tool server only the project names, or a hook from the project's layer), Eitri ends the
tab with the message "Codex loaded project configuration Eitri did not allow".

### The first start checks the Codex binary

The first time Eitri starts a `codex` it has not checked before, it runs a short scripted conversation of a few
seconds against a stand-in model on your own machine. No login or network is used, and it never talks to your
account. The conversation tries to make Codex use what Eitri switched off, using a deliberately hostile
configuration, and it passes only when none of it takes effect. While it runs the status band says "checking
this Codex… (once for this binary, a few seconds)".

- A pass is remembered for that exact binary (its path, size, modification time and version) under
  `~/.local/state/eitri/codex-check/` (inside `XDG_STATE_HOME` when that is set); an updated Codex is checked again.
- **Until the check passes, every Codex start is refused**, trusted projects included, with "Eitri could not
  confirm that this Codex keeps its levers" and the path of a log, `check.log`, which sits beside the verdict. A failure
  is not remembered, so the next start tries again.
- The check keeps files the sandbox must not let Codex write in a directory of its own. Codex's sandbox always
  lets commands write `/tmp` and `$TMPDIR`, so if your state directory is under either and neither
  `XDG_RUNTIME_DIR` nor your cache directory is elsewhere, the check cannot tell a held sandbox from a broken one
  and fails; set `XDG_STATE_HOME` to a directory outside them.

### Auto and bypass in a Codex tab

`Shift+Tab` switches the tab between Auto and Bypass as in any tab, with the same question when entering
bypass. The modes mean something different here, because Codex has its own approval flow:

- **Auto.** Codex runs under its own reviewer. When it needs to leave its sandbox, its reviewer decides. A call
  the reviewer allowed or refused gets a note on its row: `allowed by auto review: ...` or `blocked by auto
  review: ...`, with the reviewer's reason. What the reviewer does not settle reaches you as a card.
- **Bypass.** Eitri approves, without a card, every request Codex sends that would otherwise be a card; the row says
  "allowed in bypass". Entering bypass approves the cards already waiting.

Whatever the mode, Eitri's classifier, your saved rules and the "defer to Claude Code" handling of Claude tabs
do not apply to Codex; a request is a card, or answered by the reviewer, or answered as described next.

A mode change takes effect in Eitri at once and in Codex's reviewer at the next turn: a running turn keeps the
reviewer it started with. `<prefix> i` says so in the `mode lag` row.

**Approve and deny on a card.** `a` approves once and nothing is remembered; there is no "always allow". `d`
denies and the turn goes on. `D` flashes that Codex takes no reason with a denial. `Ctrl+c` interrupts the
turn, which also cancels the waiting request. A card shows what it approves: the command and its folder, each
changed file with its diff in full, or the tool server, tool and arguments. A command card says it "runs outside the
sandbox with your full rights", and a network card that it may.

**What Eitri answers itself, in every mode, with a note on the row.** Some requests are not worth a card or
cannot be shown, and Eitri declines them or grants nothing: input typed into a command that is already running;
a file change Eitri did not see in full; a request to write anywhere under a folder for the rest of the session;
a request for extra permissions (Eitri grants none); a command or tool call it cannot show; a form from a tool
server that is not a tool approval. The row reads "answered by Eitri:" followed by what Codex asked and, for most
of them, what Eitri did, for example "Codex asked for extra permissions; Eitri grants none".

### What Codex does without asking

Codex runs in its `workspace-write` sandbox with the approval policy `on-request`, and Eitri sends the narrow
form of that sandbox with every turn so that a `network_access` or extra writable folder in your own or the
project's Codex configuration cannot widen it. Inside it:

- Commands and edits inside the project run without a card. So can writes to `/tmp` and `$TMPDIR`, which Codex
  allows in this sandbox.
- **Codex can read any file you can read.** The sandbox limits writes, network and sockets, not reads.
- There is no network and no socket access inside the sandbox.
- **Commands your own exec-policy `allow` rules cover run outside the sandbox**, with no request.
- **MCP tools follow the approval mode of their server.** A tool whose server needs no approval runs with no
  request.
- A request to leave the sandbox is decided by the reviewer in Auto and approved by Eitri in Bypass. A command
  approved that way runs with your full rights.

`<prefix> i` spells this out for the tab on screen in the `runs without asking` row, and the row follows the
tab's mode.

### Interrupting, subagents and resuming

- **Interrupt.** `Ctrl+c` ends the turn at once, but Codex lets a command it already started run to its end.
  Eitri waits up to three seconds after the key. If a command, a file change or something it could not read is
  still running by then, Eitri stops the Codex session, and everything it started with it, and says so: "Codex
  kept running a command after the interrupt; its session was stopped so nothing more runs. The conversation can
  be resumed." The same happens if the turn itself has not ended three seconds after Eitri told Codex to stop it
  (Codex can refuse an interrupt that comes just as the turn starts; Eitri asks again first). A command the model left running in the background past its own call is not caught by this: the
  turn ends, the band says how many are still running, the turn's review says later changes are not in it, and
  `<leader>k` stops them by stopping the session after a `y/n` (the conversation can be resumed); a later
  interrupt or closing the tab stops them too.
- **Subagents.** Codex may start subagents and Eitri cannot turn that off. Their rows and cards say `subagent`,
  they inherit the sandbox and the trust answer of the tab, interrupting or closing the tab reaches all of
  them, and the tab counts as busy until every one of them is idle. A subagent's own text is not shown.
- **Resume.** A Codex conversation can be resumed. The resume picker and `w` list only the saved sessions of the
  agent of the tab you are in. Saved tabs are restored like Claude's (see [Getting started](getting-started.md#tabs-and-sessions)),
  but each Codex tab comes back holding its resume and starts only once Codex's own trust question is settled:
  answered, or not needed because the project has nothing to trust.
- **Usage.** The tab shows Codex's rate-limit report in `prefix i` (`limits`) and, when Codex reports it, the
  reset time in a turn's ending row. Codex tokens come without a cost, so no cost is shown.

### What a Codex tab does not have

- Slash commands are sent to Codex as typed. There is no held-back command list and no `/model` or `/effort`
  picker.
- `<leader>t` (continue in a terminal) flashes "Continuing a Codex session in a terminal is not available yet".
- `prefix i` lacks the Claude-only rows (the Claude session ids, the CLI build, permission rules, account and
  model).
- Saved permission rules, Claude Code's auto mode and the `agent.user_settings` switch do not apply.

## Other agents over ACP

Any agent that speaks ACP version 1 over standard input and output can be a tab. You register it in `init.lua`:

```lua
eitri.agent.register({
  name = "myagent",                              -- what :tabnew and agent.default call it
  command = { "my-acp-agent", "--acp" },         -- the program and its arguments
  project_files = { ".myagent/", "MYAGENT.md" }, -- what the agent reads from a project
  auto_mode = "auto",                            -- optional: a mode id of the agent
})
```

- **`name`** is required: letters, digits, `.`, `_` and `-`, starting with a letter or digit, at most 64
  characters. `claude` and `codex` are refused in any letter case, and a name can be registered once.
- **`command`** is required: a list of strings, the program first. The program must be an absolute path or a bare
  command name looked up on `PATH`; a relative path with a `/` would name a file in the project and is refused.
  Whether the program exists is not checked at startup: a missing one fails only that tab's start.
- **`project_files`** is required and must not be empty: the files and directories (a trailing `/` marks a
  directory) the agent reads from a project, one name each, with no `/` inside. Eitri uses it for the trust
  question below. List everything the agent reads: Eitri cannot know that for you.
- **`auto_mode`** is optional: the id of one mode of the agent. See [Modes](#modes-and-the-agents-own-settings).

Any other key, or a bad value, stops Eitri at startup and names the key, and a `pcall` around the call does not
hide it. Then `:tabnew myagent` opens a tab of it, and `agent.default` may name it; `agent.default` is read after
the whole file has run, so it may come before the `register` call.

### Trust: an ACP agent starts only after a yes

An ACP agent reads its own configuration from the project and Eitri has no way to ask it not to. So for a
registered agent the question works differently:

- The question covers the files you listed in `project_files`, looked for in the project directory and each
  directory above it up to the top of its git repository. A listed directory is read whole.
- It is asked **even when none of the files exist**: "myagent loads whatever project files it finds; there are
  none now. Start it here?" (with your agent's name). A `y` is remembered for that empty state, so a listed file that appears later asks
  again.
- **`y` starts the agent; `n` does not.** An `n` is not remembered, and the tab says that the agent would load this
  project's own configuration, which you have not trusted, and that Eitri cannot start it without loading that. The
  footer reads "y trusts and starts it · n does not start it · Esc puts this off".
- Nothing in the question is cut. If it would pass 2 MiB, Eitri cannot show it whole and the agent's start is
  refused with a message saying so.
- Eitri cannot tell which of the listed files the agent actually follows, so a listed file or directory that is
  a symbolic link, or a link anywhere inside a listed directory, makes the answer last one start only.
- The question and the remembered answer belong to that agent alone. A tab of a name that is no longer
  registered is asked about with no files and then refused: "was not started: no agent of that name is
  registered in init.lua".

Eitri starts the agent with your environment as it is. An agent that treats a relative path in an environment
variable as its configuration directory would read the project's directory as yours, and Eitri does nothing
about that for a generic agent. (A Codex tab refuses a relative `CODEX_HOME` for exactly this reason.)

### Modes and the agent's own settings

Eitri offers the agent no file or terminal methods and never calls its authentication step. It sets nothing
on the agent except one thing: when the registration has `auto_mode`, that mode id is set at the start of every session, in
both Eitri modes. Eitri then checks every report of the agent's mode against it: a refusal to set it, or a
difference, ends the tab with a mismatch message and nothing is answered after that. Without `auto_mode` Eitri
sets and checks nothing, and `prefix i` says in its `runs without asking` row that Eitri does not know what the
agent runs without asking.

Eitri's Auto and Bypass are Eitri's own: they decide how Eitri answers the agent's requests, at once, and never
change the agent's mode. The `mode lag` row says there is none.

### Cards

Every request that reaches Eitri is a card, in Auto; in Bypass Eitri approves it. Saved rules and Eitri's
classifier never apply.

- An approval selects the agent's "allow once" option, a denial its "reject once" option. The "allow always" and
  "reject always" options are never offered or chosen. A request with no option of the kind you press cannot be
  answered that way, and the card stays.
- A card that cannot show the whole call has no approve option: a call input over 256 KiB, diffs that together
  exceed it, or a diff whose path or text is not a string. The card says Eitri held the option back and names
  the files. `a` and `Ctrl+y` then flash "this card cannot be approved". You can still deny it or interrupt.
- A card draws every line of each diff.
- `Ctrl+c` interrupts: Eitri cancels the turn and answers every open request as cancelled. There is no local
  timeout; an agent that never ends the turn keeps the tab busy until you close it.

### Resuming

An ACP tab can be resumed and saved with the tabs only when its agent says it can load a saved session. A saved
tab comes back holding its resume and starts when its own agent's question is answered with `y`. A saved tab
whose name `init.lua` no longer registers is skipped.

### What an ACP tab does not have

Plans, command lists, session titles and other updates an ACP agent may send are not shown; Eitri has no rate
limits, no context bar, no slash-command list and no model picker for these tabs. `prefix i` has a `context` row,
which shows how full the agent's context is once the agent reports it. Tokens come without a cost.

## Known limits

- **What the agent runs without asking is the agent's own business.** For Codex the list is under
  [What Codex does without asking](#what-codex-does-without-asking). For an ACP agent Eitri shows every request
  it receives, but a request the agent never makes, because its own mode allows the action, never reaches you.
- **Your own configuration of the agent always loads**, and Eitri cannot turn it off. Trusting a project only
  adds the project's configuration to it.
- **A hook of yours can answer first.** A `PermissionRequest` hook in your own Codex configuration may answer a
  request before Eitri sees it. Whether a hook can rewrite a command after you approved it has not been
  measured.
- **Codex's sandbox reads anywhere you can.** It is not a boundary for secrets.
- **There is no Eitri-side classifier or saved rule for these agents.** The permission checks of a Claude tab
  are Claude's alone.
- **These tabs are new.** Most of what is described here has been tried against stand-ins and not on every
  setup; the [known issues](../known-issues.md#codex-and-acp-tabs) list what is open.
