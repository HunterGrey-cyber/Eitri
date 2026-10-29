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

A Claude tab also shows the work Claude Code does on its own: background tasks, dynamic workflows, subagents, the
turns it starts by itself and the questions it asks you. That comes first below, since Codex and ACP tabs have none
of it.

## What a Claude tab shows of Claude Code's own work

Much of what Claude Code does happens outside the turn you started. It runs commands and subagents in the
background, runs dynamic workflows, schedules wake-ups for later, and starts turns of its own when any of these
report back. A Claude tab shows all of it, and lets you stop it.

### Turns Claude Code starts

When a background command finishes, a monitor reports, a background subagent or workflow ends, or a scheduled
wake-up or cron task fires, Claude Code starts a turn without a prompt from you. The tab shows it as a turn like
yours. Its first row says what started it, for example `Background command "build" completed` or
`Scheduled wake-up: check the deploy`, and its tool calls, cards and reply follow. While it runs, the activity line
reads `Claude Code is handling: ...`, and a card it raises sits under its own tool row in that turn.

`Enter` while such a turn runs puts your message into it, as in any running turn. A message Claude Code keeps for
later shows as a `↪` line above the activity line until Claude Code reads it, and then appears where it was read.

### Background tasks and `/tasks`

The band counts the tab's background work in Claude Code's words: `1 shell`, `2 shells, 1 monitor`,
`1 local agent`. While no turn runs, a line above the message box says the same and ends in `/tasks`.

`/tasks` (or `/bashes`) opens the list of background tasks: shells, monitors, local agents (subagents in the
background) and dynamic workflows, then the ones that finished. `j` and `k` select a task and `Enter` shows its
detail: for a shell or a monitor, the end of its output, with escape sequences removed and hidden characters made
visible; for a workflow, its phases and agents. `x` stops the selected task, and only that one. `o` opens its output
read-only in Neovim. `q` goes back, and from the list closes it. All the keys are in
[Keys](keys.md#the-background-tasks-list).

`<leader>k` stops every background task of the tab after a `y/n` question that names them
(`Stop 2 shells, 1 monitor? y/n`). It works while no turn runs, and the conversation goes on.

`Ctrl+c` interrupts the turn and leaves its background work running: stop that from `/tasks` or with `<leader>k`.
Closing the tab, its view or the window while background tasks run asks first and says what is still running.
Scheduled wake-ups and cron tasks are not counted, so closing ends them without asking, apart from a cron task
Claude Code was told to keep across sessions (see [Not in a Claude tab yet](#not-in-a-claude-tab-yet)).

### Dynamic workflows

A dynamic workflow is a script Claude Code runs in the background, with agents of its own. When one needs your
approval, its card reads `Run a dynamic workflow?` and shows the workflow's name, its arguments and its script as
plain text; `View raw script` opens the script read-only in Neovim. While it runs, the band shows its name, how many
of its agents are done, the time and the tokens. `/workflows` opens the tasks list at the workflows, where a
workflow's detail lists its phases, then its agents with their state, model, tokens and tool calls. `x` there stops
the run. When the workflow ends, Claude Code reports back in a turn of its own.

### Subagents

The calls of a subagent Claude runs in the foreground are nested under the subagent's row, and `Enter` opens and
folds them. A subagent in the background takes one status line, with the number of tools it used, until it is done,
and its report starts a turn of its own.

### Notices, compaction and `/clear`

Claude Code's notices appear where its terminal shows them. While Claude Code compacts the conversation, the activity
line reads `Compacting conversation...`, and a divider then marks the place, with the token counts before and after.
When the model service is overloaded, the activity line shows the retry (`overloaded · Retrying in 1s · attempt
1/2`) until the reply arrives. A prompt that a `UserPromptSubmit` hook blocked shows the hook's words in a muted row.

`/clear` starts a new conversation in the same tab. The view empties, background tasks keep running and stay in the
band, and a later resume of the tab continues the new conversation.

### Long-running commands

Claude Code limits the timeout Claude may give a command. Eitri starts it with `BASH_MAX_TIMEOUT_MS` set to
2147483647, the largest value it takes (almost 25 days), so a background command that Claude starts with a long
timeout runs for as long as that timeout says. A background command started without a timeout is still stopped
after 30 minutes. That is Claude Code's own default, and Eitri keeps it on purpose. Claude is told when it happens
and can start the command again with a timeout, and the task's detail in `/tasks` says so. If you set
`BASH_MAX_TIMEOUT_MS` yourself, in your environment or in the `env` of Claude Code's `settings.json`, Claude Code
uses your value.

### Claude's questions

When Claude asks you a multiple-choice question (Claude Code's `AskUserQuestion` tool), a question box takes the
place of the message box, the activity line reads `⚑ Claude has a question`, and the tab counts the question with
its cards. Pick an option by its number, or move to it with `j` and `k` and press `Space`. Either way the box stays
on that question; in a question that takes several answers, the same keys select and clear as many as you like. In a
question with one answer, `Enter` picks the option under the cursor and moves on. `Type something` takes an answer
in your own words, and
`Ctrl+g` edits it in Neovim. `n` adds notes to an option that shows a preview. `Tab` goes to the next question; when there are
several, a review page lists your answers. `Ctrl+Enter` sends them all. `Enter` on `Chat about this` tells Claude you
would rather talk it over (its number only moves the cursor there, and `Space` does nothing on it), and `Esc` cancels the question, which
stops the turn. `Ctrl+k` goes back to the conversation with the
question still waiting, and `i` returns to it. A half-written answer survives a switch to another tab and a reload of
the page.

Only you answer a question: bypass, `Ctrl+y`, the card keys, saved rules and Claude Code's classifier never do (see
[Permissions](permissions.md#claudes-questions-to-you)). Closing the tab while a question waits asks first and says
that closing cancels it. All the keys are in [Keys](keys.md#claudes-questions).

### Resuming a conversation

A resumed conversation is rebuilt from Claude Code's own session file, and it follows Claude Code's turns. A turn
Claude Code started by itself comes back with what started it. The slash commands and `!` commands you typed come
back as your prompts. Questions come back with the answers you gave, or as declined. The `No response requested.`
line that Claude Code writes after an interrupt is left out.

A few things do not come back: a subagent's own calls, which Claude Code keeps in a file of their own, and a message
that another Claude Code session delivered into a running turn (the live tab does not show it either). A few turns
that Claude Code starts by itself for a reason Eitri does not recognise yet come back inside the turn before them.
A question that was interrupted, or whose tab was closed, may come back as declined, because that is how Claude Code
recorded it.

### Not in a Claude tab yet

Plan mode, attaching files or images to a message, and a list of the scheduled wake-ups and cron tasks. Those still
fire while the tab stays open, each as a turn of its own. Closing the tab, its view or the window ends them, and
since Eitri has no list of them it does not ask first. The only cron task Claude Code keeps after its session ends is
one it was told to keep across sessions: it saves that one in the project's `.claude/scheduled_tasks.json`, and may
run it in a later session there.

The Claude Code terminal's `Ctrl+B`, which moves a running foreground command to the background, has no
counterpart in a Claude tab. In Eitri `Ctrl+b` is the prefix key.

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

### Manual, auto and bypass in a Codex tab

`Shift+Tab` cycles the tab through Manual, Auto and Bypass as in any Claude tab, with the same question when
entering bypass. The modes mean something different here, because Codex has its own approval flow:

- **Manual.** Codex's reviewer is turned over to you: whenever Codex needs to leave its sandbox it asks, and every
  such request is a card. Manual does not reach inside the sandbox: commands and edits there still run without a
  request, as in every mode (below).

- **Auto.** Codex runs under its own reviewer. When it needs to leave its sandbox, its reviewer decides. A call
  the reviewer allowed or refused gets a note on its row: `allowed by auto review: ...` or `blocked by auto
  review: ...`, with the reviewer's reason. What the reviewer does not settle reaches you as a card.
- **Bypass.** Eitri approves, without a card, every request Codex sends that would otherwise be a card; the row says
  "allowed in bypass". Entering bypass approves the cards already waiting.

Whatever the mode, Eitri's classifier, your saved rules and the "defer to Claude Code" handling of Claude tabs
do not apply to Codex; a request is a card, or answered by the reviewer, or answered as described next.

A mode change takes effect in Eitri at once and in Codex's reviewer at the next turn: a running turn keeps the
reviewer it started with (a turn started in Manual or Bypass keeps asking about everything outside the sandbox; a
turn started in Auto keeps its reviewer). `<prefix> i` says so in the `mode lag` row.

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
- A request to leave the sandbox is a card in Manual, decided by the reviewer in Auto and approved by Eitri in
  Bypass. A command approved that way runs with your full rights.

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
- **A message while a turn runs.** `Enter` puts it into the running turn (Codex's `turn/steer`) without
  stopping it; Codex reads it at its next step (after the command or card it is waiting on, if any), and the
  conversation shows it where Codex read it. A message Codex refuses (by Codex's own source, a review or a
  compaction may refuse one; Eitri has not seen that happen) or has not read when the turn ends is held for the turn's end, with a line saying why. Codex forgets a message
  it had not read when the turn is interrupted, so Eitri sends it again with the next turn.
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

- Slash commands are sent to Codex as typed. There is no held-back command list, no `/model` or `/effort`
  picker, and no `/` command menu (Codex reports no command list to complete from).
- None of [what a Claude tab shows of Claude Code's own work](#what-a-claude-tab-shows-of-claude-codes-own-work):
  `/tasks` and `/workflows` are sent to Codex as typed, and the band only counts the commands still running in the
  background.
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
change the agent's mode. The `mode lag` row says there is none. A tab of a registered agent has no Manual mode:
the agent's own mode is set once, at the start, so Eitri cannot make it stricter afterwards. `Shift+Tab` there
switches between Auto and Bypass, and a new tab of such an agent, opened while new tabs start in Manual, starts in
Auto and Eitri says so once.

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

ACP has no way to put a message into a running turn, so `Enter` while one runs holds the message until the turn
ends. Plans, session titles and other updates an ACP agent may send are not shown; Eitri has no rate
limits, no context bar and no model picker for these tabs. The command list an agent reports is the `/` menu in
its composer (see [the composer's keys](keys.md)): the names and descriptions are drawn as plain text, and a tab
whose agent has reported none has no menu. `prefix i` has a `context` row,
which shows how full the agent's context is once the agent reports it. Tokens come without a cost.

## Chat views

The agent panel you start with is the **base view** (`agent`). A **chat view** (`agent#2` to `agent#9`) is another
page of the same panel, a module of its own in the layout, and it shows exactly one session tab; the base view shows
all the others, with its tab strip. The keys that make and use them are in [Keys](keys.md#chat-views). What matters
for an agent is this:

- **A view is only where a tab is shown.** The tab is the window's, whichever agent it runs (Claude, Codex or an ACP
  agent): it keeps running when its view is hidden, its cards wait for you, and its session is saved with the others.
  There is one account for the whole window, so every view's tab spends the account the window was started with
  ([Accounts](configuration.md#accounts)).
- **`<prefix> !`** breaks the tab you are on out into a view of its own, and from that view joins it back. The base
  view always keeps a tab, so it refuses with only one. A new view can also start empty (`instance.new { kind =
  "agent" }`, `eitri.layout.open("agent")`, or a split key then `a` from a chat); its first message asks the
  [trust question](permissions.md#trusting-a-project) in that view like any first session.
- **A card is answered only from the page that shows its tab.** The base view shows a tab until you break it out,
  and an extra view shows its tab; an answer (`a`, `d`, `D`, `Ctrl+y`, a button, the bypass `y` or the trust `y` and
  `n`) sent from any other page is refused and the card keeps waiting. The case this is for: you press an answer
  in the base view in the moment `<prefix> !` moves that tab into a view of its own; the answer does not reach the
  tab, and the card is there to answer from its new view.
- **A covered view may not answer, and it asks again.** A float over a view covers it (see [A chat under a
  float](#a-chat-under-a-float)); the check is made per view, so a float over `agent#2` refuses that view's answers and
  leaves the base view's alone. When it is the trust question that is answered from a covered page, the answer is
  refused with the reason and the question is shown again in the same page, so it is there to answer once the float is
  off. A `:trust` typed in a covered page ends as not trusted, with the reason (nothing is recorded), and typing it
  again once the page is uncovered asks the question.
- **The base view's strip lists only its own tabs**, so a tab shown in another view leaves a gap in the numbers
  (the chooser, `w`, lists every tab and names the view of a tab shown in one: ` · agent 2`). A view with one tab
  draws no strip.
- **A restored tab comes back in its view** when the layout still has that view and no other tab has claimed it;
  otherwise it comes back in the base view. A view that is left with no tab, once the restore has finished, is
  removed.
- **Each view carries its own attention.** Its chip and strip entry read `agent 2 ⚑1` while a card waits in that
  view's tab, a new card in a hidden view is revealed under `modules.chat.on_permission = "reveal"` as it is for the
  base view, and the toast names that view and its way back.

## A chat under a float

The agent panel, or any [chat view](#chat-views), can sit under a [float](keys.md#floats-the-scratchpad-and-workspaces) (a terminal in the scratchpad,
say). A chat that a float covers by any amount is still running, and it is treated as covered rather than shown:

- **Its chip is back in the top bar.** While a card waits it reads `agent ⚑N`, as it does when the chat is hidden.
  A turn that finishes while it is covered leaves `agent •`, which goes once the float is off the chat.
- **A card cannot be answered from it.** A card answer from the covered page (`a`, `d`, `D`, `Ctrl+y`, a button),
  or the `y` that enters bypass mode, is refused and the card stays waiting. The app name flashes, and the toast and
  the page say which float covers it and the key that clears it, pressed with the keys in that float:
  `agent is under terminal 2: <prefix> Alt+- from terminal 2 stows it` for a scratch float, `agent is under
  terminal: <prefix> Alt+Shift+Space from terminal sinks it` for any other (the action's name when it has no key).
  From the chat, the first `<prefix> Alt+-` moves the keys into a shown scratch float and the second stows it. The
  check is made when you answer, against where the floats are then, so the answer goes through the moment the
  float has moved off or been put away.
- **With `modules.chat.on_permission = "reveal"`**, a new card puts away the scratch floats that cover the chat,
  except the one that has the keys (the keys do not move). A float that still covers the
  chat after that, or any cover with the default `"badge"`, gets the badge and the toast.

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
