English | [简体中文](permissions.zh-CN.md)

# Permissions and trust

This page says what happens to a tool call before it runs, who answers it, what a saved rule can and cannot do,
what Claude Code itself may still ask, and how Eitri decides whether to load a project's own Claude Code
configuration. It describes what Eitri does today, including the places where it is weaker than you might
assume; those are collected under [Known limits](#known-limits).

How to answer a card with the keyboard is in [Getting started](getting-started.md) and
[Keys](keys.md#answering-a-card).

This page is about Claude tabs. A tab that runs Codex, or an agent you registered, has its own sandbox,
reviewer and trust question; they are described in [Codex and other agents](agents.md).

The agent's [editor tools](editor-tools.md) (`mcp__eitri__open_file` and the others) are outside all of this, in every
kind of tab: neither saved rules, nor Manual's or Auto's rules below, nor Claude Code's auto mode judge them. Eitri's
own table for them is [on their page](editor-tools.md#cards), and so are the ways their cards differ.

## Who decides a tool call

Every tool call Claude Code wants to run is sent to Eitri first, in every mode, before it runs. What happens
next depends on the tab's mode and on how Claude Code is running. A tab is in one of three modes, and
`Shift+Tab` (or `<leader>m`) cycles them in Claude Code's order: manual, auto, bypass, then manual again. A new
tab starts in auto unless you set `agent.default_mode`
([How a launch starts](configuration.md#how-a-launch-starts)). While Claude Code plans, [plan mode](#plan-mode) changes
some of the answers below.

### Manual

Manual is the strictest mode. Eitri's own rules answer every call, whatever Claude Code is running: the rules
described under [Auto, when Claude Code is not running its auto mode](#auto-when-claude-code-is-not-running-its-auto-mode),
with one difference: **every file edit or write made with Claude Code's file tools (`Write`, `Edit`,
`NotebookEdit`) is a card**, inside the project too. Nothing is left to Claude Code's auto mode, so no row says
`blocked by auto`. Reading and searching inside the project and read-only commands still run without asking,
your [saved rules](#saved-rules) still apply (a shell command that writes is judged by its words, as in auto, so
a rule that allows it still does), and Claude Code's own questions are treated as in auto
([When Claude Code itself asks](#when-claude-code-itself-asks)).

Measured against Eitri's own rules, manual is never more permissive than auto: it only turns some of what they
allow into cards. Where Claude Code runs its own auto mode, an Auto tab does not use those rules: it leaves every
call no saved rule allows to Claude Code's classifier, which may refuse it. A Manual tab answers those calls
itself, so reading inside the project, read-only commands, subagents (`Task`, `Agent`), `ToolSearch` and
`TodoWrite` run in manual without Claude Code's classifier seeing them (an approval from Eitri skips it), where
in auto that classifier could have blocked one. Everything else Eitri's rules would put on a card is a card in
manual too.

Claude Code's scheduling and task tools run without a card in every mode, as Claude Code itself runs them without
asking: `ScheduleWakeup`, `CronCreate`, `CronList`, `CronDelete`, `ListAgents` and `TaskStop`. What they schedule
shows up later as a turn of its own while the tab stays open. Closing the tab ends it, except a cron task Claude
Code was told to keep across sessions, which it saves in the project and may run in a later session there (see
[Codex and other agents](agents.md#not-in-a-claude-tab-yet)).
`SendMessage`, which reaches another session, is still a card wherever Eitri's rules decide.

### Auto, when Claude Code runs its own auto mode

This is the normal case. Eitri asks Claude Code to run its own auto mode and then lets it judge the calls:

- A call that one of your [saved rules](#saved-rules) allows is approved at once, and the tool's row says which
  rule did it.
- Every other call is left to Claude Code's auto mode, which decides with its own classifier. Eitri draws no card
  for it and does not apply its own checks.
  The exceptions are a project that is [no boundary](#a-folder-that-is-no-boundary): there Eitri's own rules answer,
  as in a Manual tab; and a write into [Eitri's own configuration](#eitris-own-configuration), which is a card.
- When Claude Code refuses a call, the tool's row says `blocked by auto: <reason>`. After repeated refusals
  Claude Code asks you itself, on a card.

### Auto, when Claude Code is not running its auto mode

Claude Code can fall back to its ordinary default mode, and an installed setup may be unable to offer the auto
mode at all. Then Eitri's own rules decide which calls need a card. They are deliberately cautious: whatever
Eitri cannot be sure about is a card.

Choosing a model that cannot run auto mode ends it for that session, and Claude Code does not go back to it when
you change the model again. The tab moves to manual at that moment and says why, also in the middle of a turn. You
can cycle it back to auto, which then means the rules below, until a new session starts; moving the tab to another
window puts it back in manual.

- Reading and searching inside the project run without asking, and so do commands made of a program that only
  reads, with plain arguments. A command with a redirect, a pipe, `;` or `&&`, a glob, quotes, `$(...)`, a path
  outside the project, or a `git` option that points it at another repository is a card.
- Editing or writing a file inside the project runs without asking, except for the files and directories Claude
  Code protects: anything under `.git`, `.claude`, `.vscode`, `.idea`, `.husky`, `.cargo`, `.devcontainer`,
  `.yarn` or `.mvn`, and files such as `.gitconfig`, `.bashrc`, `.zshrc`, `.profile`, `.envrc`, `.npmrc` and
  `.mcp.json`. Those are cards.
- `WebFetch`, `WebSearch` and `MultiEdit` always ask. A tool Eitri does not know asks. A few tools that change no
  file, run no command and reach no network never ask.
- A project directory that is no boundary (your home directory or above it, and the other cases under
  [A folder that is no boundary](#a-folder-that-is-no-boundary)) makes every call that has to be judged against
  the project ask.
- A [saved rule](#saved-rules) can allow a command that only fails the "known to be read-only" test.

### Bypass

In bypass Eitri approves every call itself, without a card. That includes Claude Code's own questions (the
[next section](#when-claude-code-itself-asks)), even the ones your own `permissions.ask` rules force: in a bypass
tab an ask rule stops nothing. The one exception is [`nvim_exec`](editor-tools.md#nvim-exec), an editor tool that is
off until you turn it on: every call of it is a card, in bypass too. Because that is a lot of trust, `Shift+Tab` into bypass asks first. The question is
a `y/n` that says what else changes: if cards are waiting in the tab it reads "Switch to bypass and approve the N
waiting cards?", and every waiting card is counted, except a card from an agent's
[editor tools](editor-tools.md#cards), which entering bypass never answers. The `y` counts only when it is pressed
on its own and a moment after the question appears, and only the cards still waiting at that moment are approved.
Bypass is entered only from auto. Leaving bypass needs no question and lands in manual.

Bypass never answers a question Claude asks you ([Claude's questions to you](#claudes-questions-to-you)), never
settles a plan, and approves nothing by itself while Claude Code plans ([Plan mode](#plan-mode)).

Starting in bypass without the question is possible only by saying so in advance, with
`agent.default_mode = "bypass"` in your `init.lua`; see
[How a launch starts](configuration.md#how-a-launch-starts).

### What Claude Code itself runs in

Claude Code never runs in its own bypass mode under Eitri: it runs in its default or its auto mode, with Eitri in
front of it, and "bypass" is only Eitri answering for you. Eitri also checks which mode Claude Code reports. If
a session reports a mode Eitri did not ask for and does not expect, such as Claude Code's own bypass mode, Eitri
ends that session with a message and answers nothing further in it.

### Eitri's own configuration

What Eitri reads when it starts takes effect the next time you open a window, and nobody is asked then. So a call
that writes into one of these places is a card in manual and in auto, also where Claude Code runs its own auto mode,
and no saved rule allows it:

- your configuration directory, `~/.config/eitri` (and `$XDG_CONFIG_HOME/eitri` or `$EITRI_CONFIG_DIR` when you set
  them): `init.lua`, what it `require`s from `lua/`, `neovide.toml`;
- `~/.local/share/eitri` (and `$XDG_DATA_HOME/eitri`): your own plugins, which win over the shipped ones, and the
  Neovim and the agent sidecar Eitri installed for itself;
- `~/.local/state/eitri` (and `$XDG_STATE_HOME/eitri`): your saved rules, the projects you trusted, the mode each
  project remembers, layouts;
- the directory the `eitri` program runs from, which holds the shipped plugins and the sidecar, and for the
  installer's copy `~/.local/lib/eitri` and the `~/.local/bin/eitri` launcher;
- `eitri` under each system data directory the plugin search reads (`$XDG_DATA_DIRS`, or `/usr/local/share` and
  `/usr/share`), where an installed package's plugins live.

Eitri follows links on the way, so a link in your project that leads into one of them counts, and so does a
configuration directory that is itself a link into a dotfiles repository you have open. It also follows the links
inside these directories back to where they lead: when a dotfiles manager linked `~/.config/eitri/init.lua` to a file
in your dotfiles repository, a write to that file is a card too. A file with a second name (a hard link) is a card,
since that other name could be one of these files. If your project is one of these directories, lies inside one, or
is where a link in one leads, every edit and command in it is a card. This check leaves reading alone (a read outside
the project asks for its own reason), and bypass allows the writes as it allows everything else.

A shell command is judged by its text, as everywhere in Eitri: one that names one of these directories
(`~/.config/eitri`, `$HOME/.local/share/eitri`, the full path, or a variable such as `$XDG_CONFIG_HOME`) is a card.
One that gets there another way, a script or a `cd` first, is not seen; see [Known limits](#known-limits).

## A folder that is no boundary

Eitri's own rules judge a path against the project directory. Some folders cannot be that boundary, and then every
call that has to be judged against the project asks. A folder is no boundary when it is:

- the filesystem root, a kernel tree (`/proc`, `/sys`, `/dev`) or a folder that has moved since it was resolved;
- your home directory or above it;
- not yours (another user owns it, which is the case for `/etc`);
- writable by other users (`/tmp` and `/var/tmp`, or a shared tree). A folder only your own group can write to
  still counts, when that group has no other member: the `drwxrwxr-x you you` checkout that a umask of 0002 makes
  stays a boundary.

You meet this most often with [`eitri FILE`](getting-started.md#open-a-file) on a file that has no project of its
own, such as `eitri ~/notes.md`. It applies to every window on such a folder, however it was started, so
`eitri /tmp` and `eitri ~` behave the same way. The panel says so in a note, with the reason.

- **Manual and Auto Claude tabs ask.** Every call that would be judged against the project is a card. An Auto tab
  that would otherwise leave the call to Claude Code's own classifier is answered by Eitri's rules instead, which
  ask here.
- **Bypass does not ask**, as everywhere: you chose it.
- **A Codex tab starts read-only.** Nothing is written from inside its sandbox, and a write has to ask to leave it,
  through Codex's own approval flow. That is a card in Manual and in Auto, because at such a folder Codex's own
  reviewer does not decide; Bypass answers the request itself. The project's own Codex configuration does not load
  there, whatever you answer to the trust question: a rule or a hook in the folder could take a command out of the
  sandbox with no card. What your own exec-policy `allow` rules cover still runs outside the sandbox, and MCP tools
  still follow their servers' approval modes. The folder is looked at again at every turn.
- **A tab of a registered agent keeps its own rules.** ACP has no sandbox for Eitri to ask for, and Eitri cannot tell
  what an agent's modes mean. A registration can name a [`read_only_mode`](agents.md#other-agents-over-acp), which is set
  instead of `auto_mode` at such a folder, once, at the start. Without one the agent's mode stands.

## Saved rules

A permission card for a plain shell command can carry a third button, "Always allow `git log *` in this
project", between Approve and Deny. Pressing it, or `2` in card mode (where Deny is then `3`), saves a rule and answers the card. The button belongs to the cards Eitri's own checks
draw. Where Claude Code runs its auto mode, which is the usual case, an Auto tab hands most calls to that mode
instead of drawing a card, so you may rarely see the button there; a rule then comes mainly from a Manual tab,
a session where Claude Code runs in its default mode, or from editing the rules file by hand.

- **What a rule looks like.** It is written the way Claude Code writes it, `Bash(git log *)`, and matches whole
  leading words: `cargo test *` allows `cargo test --release` but not `cargo testx`. Only `Bash` has rules; there
  is none for any other tool.
- **What the button offers.** The first word of the command plus the second when that is a plain word, so
  `git log --oneline` offers `git log *` and `uname -a` offers `uname *`. When an option comes before a later
  plain word (`git -C sub log`) nothing is offered, because the first word alone would cover every subcommand.
  The button appears only where a rule could take effect: for a command that asks only because its first word
  (or its `git` subcommand) is not known to be read-only. A command that asks for any other reason has no third
  button.
- **What can never be a rule.** A rule whose first word is a program that runs another program, matched by name
  or by path (`/usr/bin/timeout` counts), or whose first word contains `=` (`LC_ALL=C cargo test`, which the
  shell reads as an assignment followed by the real program). A rules-file line like that is skipped. The
  programs are:

  ```text
  bash busybox chroot chrt command dash doas env eval exec fish flock ionice ltrace nice nohup
  runuser setsid sh stdbuf strace su sudo taskset time timeout unbuffer watch xargs zsh
  ```

- **A rule never makes a command run that Eitri cannot read plainly.** A rule replaces only the verdict "this
  program is not on the read-only list". A command with a redirect, pipe, `;` or `&&`, glob, quotes or `$(...)`,
  a path outside the project, or `git -C` and similar options is not allowed by a rule even when the rule's words
  match it. Where Claude Code runs its auto mode (the usual case), such a call is left to that mode's classifier,
  which may run it without any card; otherwise it is a card.
- **A rule covers more than the button offers.** The button's caution decides only what it suggests. A saved
  rule allows every plainly readable command that starts with its words, including ones the button would have
  offered no rule for: with `cargo *` saved, `cargo -p helper run` runs without a card too. Save or write the
  narrowest rule that does the job.
- **Where rules apply.** In a Manual tab, and in an Auto tab in both cases above. In bypass no rule is needed. A rule never
  answers one of Claude Code's own questions (next section).
- **Where they are stored.** Outside the project, one file per project under `~/.local/state/eitri/permissions/`
  (`$XDG_STATE_HOME/eitri/permissions/` when that is set). Eitri writes nothing into the project, unlike Claude
  Code's own "don't ask again", which writes `.claude/settings.local.json`. To remove a rule, edit that file:
  `<prefix> i` shows its path in the `permission rules` row. A window reads the file once, when it starts, so
  a removed rule stays in force in every window that is already open until you restart it. A file Eitri cannot use is set aside with
  `.unusable` added to its name and read as no rules, which means more cards, never fewer.

## When Claude Code itself asks

After Eitri lets a call through, Claude Code may still ask about it on its own. No such question is ever
answered by a saved rule or handed to Claude Code's classifier.

**In a bypass tab every one is answered for you**, whatever it says about why it asks, and the tool's row says
so under the question's label, for instance `your ask rule: Write — allowed in bypass`. Entering bypass approves
the ones already waiting as well.

**In an Auto or Manual tab it arrives as a card whose label says who asked.** These are always a card there:

- A question forced by a `permissions.ask` rule of yours that names a tool. The card says "your ask rule:" and
  the tool, for instance `your ask rule: Write`.
- A question that gives no reason. The card says "Claude Code asked (maybe your ask rule)". Claude Code names
  an ask rule only when it is a bare tool name; a rule written with a pattern, such as `Bash(echo:*)`, reaches
  Eitri as a question with no reason, so Eitri cannot tell it from any other unexplained question and shows it.
- A question of a kind this version of Eitri does not recognise.

**A question that gives its reason** (the card says "Claude Code asked" and shows the sentence): in an Auto or
Manual tab Eitri answers it only if you approved that same call, with that same input, on a card a moment before,
and only once; otherwise it is a card. Because in an Auto
tab Claude Code's own auto mode normally decides, there is usually no earlier approval, so such a question is
a card there; in a Manual tab you usually approved the call on Eitri's own card first. Where Claude Code's auto mode is running, its check on sensitive files (`.git/`, `.claude/`) does
not ask at all and its classifier decides; the sensitive-file question appears when Claude Code runs in its
default mode.

One limit to know about: Eitri cannot tell Claude Code's own reason from a reason a hook gave. A hook of yours
(or of a trusted project) that asks with a reason is treated like any other question with a reason, so in an
Auto or Manual tab your approval of that same call on a card answers it too.

## A dynamic workflow's card

A card for Claude Code's `Workflow` tool reads `Run a dynamic workflow?`. It shows the workflow's name (or the path
of its script), its arguments and its script, all as plain text, and `View raw script` opens the script read-only in
Neovim. You approve or deny it like any other card. There is no "don't ask again" for a workflow, and the card lists
no phases before you approve it. Who answers the card, and what Auto leaves to
Claude Code, is the same as for any other tool.

## Claude's questions to you

Besides asking for permission, Claude can ask you a multiple-choice question (Claude Code's `AskUserQuestion`
tool). Such a question is never a permission card, and only you answer it, with keys you press in its question box
([Keys](keys.md#claudes-questions)). Bypass does not answer it, entering bypass does not either, and neither do
the card keys, a saved rule or Claude Code's classifier. When a card and a
question wait together, those keys answer only the card.

- A question box under a [float](agents.md#a-chat-under-a-float) refuses to send, as a covered card does, and keeps
  your picks; it sends once the float is off.
- The question, its options and its previews are drawn as plain text, and hidden or direction-changing characters
  are shown as visible escapes. A preview's Markdown is drawn with no images, scripts or controls.
- Closing the tab while a question waits asks first and says that closing cancels the question.

## Plan mode

How to enter plan mode and decide on a plan is in [Codex and other agents](agents.md#plan-mode). While Claude Code
plans, Eitri answers differently:

- **No file edit or command is approved automatically, in any mode, bypass included.** Claude Code writes its own plan
  file. Every other `Write`, `Edit` or `NotebookEdit` call and every `Bash` command is a card that only you answer: no
  saved rule, no bypass and no sweep of waiting cards answers it, and it stays a card after plan mode ends, whatever
  the tab's mode is then. In a folder that is [no boundary](#a-folder-that-is-no-boundary) the plan file's write is a
  card too, and approving it can bring Claude Code's own question about the same call as a second card.
- **Claude Code's own questions are always cards** while it plans ([When Claude Code itself asks](#when-claude-code-itself-asks)),
  bypass included.
- **Other tools are judged as in the mode plan mode started from.** In Manual and Auto that is the usual answer. From
  Bypass nothing is approved by bypass: where Claude Code runs its auto mode the call goes to Claude Code's classifier,
  and otherwise it is a card. Eitri's own [editor tools](editor-tools.md) answer a Bypass tab as an Auto one while it
  plans.
- **The tab's mode stays put.** `Shift+Tab` and `<leader>m` change nothing, bypass cannot be entered, and nothing
  approves the waiting cards in bulk. After an approval, any request that arrived while plan mode was ending is judged
  once, under the tab's new mode.
- **A plan is not a permission card.** Only your choice in the plan box settles it; the card keys, `Ctrl+y`, bypass and
  saved rules never do.

Eitri tells a call made in plan mode from one made outside it by the order in which Claude Code reports them. For a
file edit, a command or a question of Claude Code's own, and for every call in a Bypass tab, it waits until it has
seen Claude Code start the call before it answers. That wait is usually about 0.2 seconds and was under one second in
every recording. A call Eitri never sees start, such as a call by one of a
[dynamic workflow's](#a-dynamic-workflows-card) agents, waits 2 seconds and is then judged both ways, as if Claude Code
were planning and as if it were not; where the two answers differ, it is a card. This applies on every Claude tab that
offers plan mode, also when nobody is planning. So in a Bypass tab every call of a workflow's agents is a card, and in a
Manual or Auto tab so is every file edit, command and Claude Code question of theirs that a saved rule or Claude Code's
auto mode would otherwise have handled.

What plan mode does not stop is under [Known limits](#known-limits).

## Trusting a project

A project can ship its own Claude Code configuration: hooks, MCP servers, permission rules. Loading it means
running whatever the repository says, so Eitri does not load it until you have seen it and said yes.

**What is looked at.** In the project directory and in each directory above it, up to the top of its git
repository (and never above your home directory): `.claude/` (all of it, not only the settings files),
`.mcp.json`, `CLAUDE.md` and `CLAUDE.local.md`. `.claude/worktrees/`, where Claude Code keeps checkouts of its
own, is named but not read. Claude Code also
loads memory files and MCP servers from the directories above the repository, all the way up, so those are
looked at too: `CLAUDE.md`, `CLAUDE.local.md`, `.mcp.json`, `.claude/CLAUDE.md` and `.claude/rules/` (not the
`.claude/` in your home directory, which is your own configuration). They are listed as `../` paths, and a
file there that belongs to neither you nor root cannot be checked (see below). A
`CLAUDE.md` in a subdirectory of the project is not looked at, and the question says so: once trusted,
Claude Code reads one when it works on a file in that subdirectory. A project with none of these starts at
once, with no question.

**The question.** Before the first session starts in such a project, the panel shows "Trust this project's
Claude configuration?" with what it found, file by file:

- each hook, with its event, matcher and command (and the handler's other fields);
- each MCP server, with its command line (environment first) and its other fields;
- each environment variable a settings file sets, as `NAME=value`;
- an `apiKeyHelper`, each `permissions.allow` rule (with a note that Claude Code applies such a rule only when it
  also trusts the folder), each `additionalDirectories` entry and each `CLAUDE.md`;
- any other setting as written;
- every other file under `.claude/` by name, directory and size, with a note when a Markdown file sets `hooks`,
  `allowed-tools`, `permissionMode` or `mcpServers` in its frontmatter.

**Nothing in the question is cut.** What `y` approves is what the box shows, whole, however long a hook command
is. Hidden and direction-changing characters in those texts are drawn as visible escapes, so a command cannot
read as something else than it is.

A configuration whose question would pass 2 MiB cannot be shown whole, and Eitri trusts nothing unseen. For a
Claude tab the panel then asks a short question that takes only `n` (start without the project's configuration)
or `Esc`; a `y` is refused. `:trust` says the configuration is too large to show and trusts nothing. Such a
project cannot be trusted in Eitri at all.

- `y` trusts the project and loads its configuration, except for a Codex tab at
  [a folder that is no boundary](#a-folder-that-is-no-boundary), which starts without it all the same.
  `n` starts the session without it. `Esc` puts the
  question off: a first message goes back to the box, and a resume is dropped with the message "not started:
  the trust question was put off". A restore put off this way keeps its sessions: the tabs come back as failed
  tabs that still hold their conversations (`r` in a tab starts it again), and a restore you asked for from the
  empty tab's `Restore last session` line stays on offer there as long as the tab is still empty (otherwise its
  tabs come back as failed tabs too). `y` and `n` count only when pressed on their
  own and a moment after the question appears; a key typed too soon flashes "wait a moment, then y or n".
- **`y` is remembered for the project and tied to exactly what you saw.** Any change to those files asks again
  and lists what was added, removed or changed. That includes an edit the agent makes under `.claude/` and
  Claude Code's own "don't ask again", which writes `.claude/settings.local.json`. It is also tied to how much
  the question showed: when a release makes the question show more than before, every project you had trusted
  is asked once more, even if nothing in it changed.
- **A change to an instruction file shows as a diff.** When `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`, a
  Markdown file under `.claude/rules/`, `AGENTS.md` or a registered agent's named file differs from what you last
  trusted, the question shows the change below everything else it lists, with unchanged lines folded. Eitri compares
  against a copy it kept at your last `y`. A file with no copy to compare (new since then, or you last answered on an
  older version), over 256 KiB then or now, past the 1 MiB kept for a project, not text, or reached through a link gets
  one line saying so. At most 2,000 rows are drawn across all the files, smallest changes first, so a large change
  cannot push a small one off the screen: a file left out says `changed, not drawn` with how many lines were added and
  removed, and one too long on its own says that. Near the question's own 2 MiB limit the largest diffs give way to the
  same kind of line, one at a time, and at the very limit the section is left out.
  The question is the same question either way. Hooks, MCP servers and settings are always shown whole, so they need no diff.
- **`n` is remembered only for this window**, for that same content, and only when Eitri could check everything
  (see the next point); otherwise the question comes back at the next start in the window. Another window asks
  for itself.
- **Something Eitri cannot check makes trust last one start only.** A file over 4 MiB, an unreadable one, a pipe
  or other special file, a settings file or `.mcp.json` that is not valid JSON ("could not be parsed for this
  check"), a symbolic link Eitri cannot follow in place of a configuration file or below `.claude/`, a file above
  the repository that belongs to neither you nor root, or a project with too many files, is named in the question
  as "cannot be checked", and `y` then covers that one start; nothing is saved and the next start asks again. On
  a Claude tab, a link below `.claude/`, or in place of `CLAUDE.md` or `CLAUDE.local.md`, is followed
  when its relative target stays inside the repository (or the project folder, when it is not in one), and so is
  a link inside `.codex/skills/` on a Codex tab: Eitri checks what it reaches and shows the link with its target,
  so a skill linked between `.claude/skills/` and `.codex/skills/`, or a `CLAUDE.md` that links to `AGENTS.md`, is
  remembered like any other file. A link that leaves, an absolute target and a loop cannot be checked. No link is followed
  when your home folder is inside the repository, or when there is a `.git` above the project and none of them
  can be verified.
- **A very large configuration is remembered by this window only.** If the record of an answer would pass
  1 MiB (roughly 1,900 files with long names), the question says so and `y` holds for this window; no record is
  written, so a new window asks again.
- Starting a session in any other way, restoring tabs or resuming a conversation, waits for the answer too.

**Commands.** `:trust` on the panel's command line (`:` in browsing; see
[the command line](keys.md#the-command-line)) shows the same question and records nothing until you answer it.
`:untrust` forgets the answer at once, in this window too. Both apply to sessions started afterwards: a running session keeps what
it loaded.

**Where it is kept.** One small record per project under `~/.local/state/eitri/trust/` (`$XDG_STATE_HOME/eitri/trust/` when that is
set to an absolute path), and beside it a copy of the instruction files as you last trusted them, `CLAUDE.local.md`
included: at most 1 MiB per project and agent, usually about the size of those files, readable only by you, and
never sent anywhere. `:untrust` deletes both. Deleting that directory forgets every answer for windows started afterwards. A window
that already has your answer keeps using it until you run `:untrust` or restart it. Eitri does not read the trust answer of terminal `claude` and terminal `claude` does not
see Eitri's, so a project you trusted in a terminal asks once more here.

## What an agent session loads

Like `claude` in a terminal, every session loads your own Claude Code settings: your hooks, plugins, skills,
`CLAUDE.md` and permission rules. To leave them out, put this in
`$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`):

```lua
eitri.config.set("agent.user_settings", false)    -- true (the default) or false
```

Any other value is listed when Eitri starts, naming the setting, and your own configuration is not loaded until you fix it.

A project's own configuration loads only after you trust it (the previous section). So what a session loads
depends on those two choices:

| `agent.user_settings` | project trusted | the session loads |
|---|---|---|
| `true` (default) | no, or nothing to trust | your user settings only |
| `true` (default) | yes | your user settings, then the project's own `.claude/`, `.mcp.json` and `CLAUDE.md` |
| `false` | no, or nothing to trust | nothing from either |
| `false` | yes | the project's own settings only |

`<prefix> i` shows this for a tab in its `settings` and `trust` rows. A tab keeps what its session was started
with; a change applies to sessions started afterwards.

One more condition applies to the project's own `permissions.allow` rules. Claude Code itself ignores them unless
it also trusts that directory in a terminal; see [Known limits](#known-limits).

Turn review, the other thing the panel can show after a turn, is described in [Reviewing a turn](turn-review.md).

## Known limits

Most of these are also in the [known issues](../known-issues.md#security), and they are the places where the behaviour
above is weaker than it may sound.

- **A command can reach Eitri's own configuration without naming it.** Eitri reads a shell command's text, so a
  command that writes there through a script, a `cd` first or a path it builds is not a card for that reason
  ([Eitri's own configuration](#eitris-own-configuration)). In an Auto tab, Claude Code's auto mode decides it, as it
  decides a write to `~/.bashrc`.
- **A hook can change a call after it was approved.** A hook in your own Claude Code settings, or in a trusted
  project's, can rewrite a tool call's input after Eitri has seen the original. When Claude Code then asks, its
  card shows the rewritten input and the tool's row shows the original. Hooks work this way in a terminal too.
- **In an Auto tab, Claude Code's auto mode can write under `.git/` without asking**, a new git hook included.
  A Manual tab puts every `Write`, `Edit` and `NotebookEdit` call on a card, but not every write: a shell command
  is judged by its words, so a saved rule such as `Bash(cp *)` still lets `cp x .git/hooks/pre-commit` run.
  Turn review does not show changes under `.git/`, and the trust question watches `.claude/`, `.mcp.json` and
  `CLAUDE.md`, not `.git/`. After a turn in a repository you care about, `ls .git/hooks` is worth a look. An
  edit under `.claude/` does make Eitri ask the trust question again before the next session.
- **A trusted project's own `permissions.allow` rules reach an Auto tab only if you also trusted that
  directory in terminal `claude`.** That is Claude Code's own rule. The rules in your user settings always apply.
- **Trusting a project means trusting its code to run.** A script a hook calls from outside `.claude/` is covered
  only through the hook's command text, as building the project is.
- **Bypass answers your own ask rules too.** In a bypass tab a `permissions.ask` rule, yours or a trusted
  project's, stops nothing: Eitri answers the question it raises like any other.
- **A message from another Claude Code session is not shown.** When another session sends this one a message while a
  turn runs, Claude Code delivers it into that turn and Claude reads it, but the tab shows no row for it, live or
  after a resume.
- **A reason is not proof of origin.** In an Auto or Manual tab, a question with a reason is answered on your approval of
  the same call, and Eitri cannot tell Claude Code's own check from a hook that asks with a reason.
- **Plan mode is supervision, not a sandbox.** It keeps file edits and commands on cards while Claude Code plans, but
  your hooks (and a trusted project's) still run, MCP servers stay loaded, and any other tool, an MCP server's included,
  can still run wherever the rules above let it. Which calls count as made in plan
  mode rests on the order in which Claude Code reports them: a call shown more than 2 seconds late is judged both
  ways, a card where the answers differ.
- **On a tab that offers plan mode, calls Eitri never sees start wait 2 seconds and may become cards.** The calls of a
  dynamic workflow's agents are the common case: in a Bypass tab every one of them is a card.
