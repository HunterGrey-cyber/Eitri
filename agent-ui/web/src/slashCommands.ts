/** Spec §9.2 (P10): whether a typed slash command is actually sent, and what the panel says when it
 *  is not. The table below is transcribed by hand from `docs/canonical/2026-09-27-slash-commands.md`
 *  (Task 10's real-CLI classification, sidecar backend, CLI 2.1.283, `ANTHROPIC_MODEL=haiku`) -- see
 *  that doc for the evidence behind every row; nothing here re-derives it.
 *
 *  Spec §9.1's five classes: **works** (the command's own effect is observable), **sent-as-text**
 *  (the model answered the literal words), **no-op**, **error**, **hangs**. None of the 28 commands
 *  Task 10 ran landed in the last three -- they exist here only because spec §9.2 names them as a
 *  holdback reason, for a future row this table might gain. */
export type SlashCommandClass = "works" | "sent-as-text" | "no-op" | "error" | "hangs";

export interface SlashCommandEntry {
  /** Without the leading "/", lower-case (the CLI's own commands are lower-case; anything else is
   *  simply not in this table and is sent as an unknown command, per spec §9.2's last bullet). */
  name: string;
  class: SlashCommandClass;
}

/** The doc's own row order, preserved here because `worksSlashCommands` below relies on it (spec
 *  §9.2: the `?` overlay lists the **works** rows -- this order is what makes that list stable and
 *  match `docs/canonical/2026-09-27-slash-commands.md` at a glance). */
export const SLASH_COMMANDS: readonly SlashCommandEntry[] = [
  { name: "help", class: "sent-as-text" },
  { name: "clear", class: "works" },
  { name: "compact", class: "works" },
  { name: "cost", class: "works" },
  { name: "context", class: "works" },
  { name: "usage", class: "works" },
  { name: "status", class: "sent-as-text" },
  { name: "model", class: "works" },
  { name: "config", class: "works" },
  { name: "memory", class: "sent-as-text" },
  { name: "init", class: "sent-as-text" },
  { name: "review", class: "sent-as-text" },
  { name: "security-review", class: "sent-as-text" },
  { name: "pr-comments", class: "sent-as-text" },
  { name: "todos", class: "sent-as-text" },
  { name: "export", class: "sent-as-text" },
  { name: "mcp", class: "works" },
  { name: "agents", class: "works" },
  { name: "hooks", class: "sent-as-text" },
  { name: "permissions", class: "sent-as-text" },
  { name: "add-dir", class: "sent-as-text" },
  { name: "resume", class: "sent-as-text" },
  { name: "rewind", class: "sent-as-text" },
  { name: "login", class: "sent-as-text" },
  { name: "logout", class: "sent-as-text" },
  { name: "doctor", class: "works" },
  { name: "release-notes", class: "sent-as-text" },
  { name: "output-style", class: "works" },
];

const BY_NAME: ReadonlyMap<string, SlashCommandEntry> = new Map(SLASH_COMMANDS.map((entry) => [entry.name, entry]));

/** Spec §9.2's own list: held back regardless of the table's class above, because a real
 *  interactive terminal opens a picker/wizard for these that a headless turn cannot draw --
 *  `docs/canonical/2026-09-27-slash-commands.md`'s "Overlap with spec §9.2" note records that all
 *  four degraded to a plain-text summary instead when Task 10 sent them anyway.
 *
 *  `/model` used to be held back only WITHOUT an argument (the spec's own reasoning: a real
 *  interactive terminal opens a menu there). Owner trial item 2 (2026-09-28,
 *  `the private review notes` §2, agreed per the same day's probe): a bare
 *  `/model`/`/effort` prints real, parseable text headless (`"Current model: ... Usage: /model
 *  <name>. Available: ..."` / `"Usage: /effort <low|medium|high|xhigh|max|auto>"`), so this panel
 *  now sends it too and opens a picker built from that reply (`../components/SlashPicker`,
 *  `./slashPicker`'s `parseModelReply`/`parseEffortReply`) rather than refusing it -- `/model` is
 *  therefore no longer in this set at all. `/effort` was never in it (it has no row in
 *  `SLASH_COMMANDS` -- an unknown slash word already falls through `heldBackSlashCommand` unheld,
 *  the same route `/model` now takes). Dated record, 2026-09-28 (v1 trial, item 2). */
const INTERACTIVE_ONLY_UNCONDITIONAL: ReadonlySet<string> = new Set(["login", "config", "resume"]);

/** Classes that are always held back, regardless of name (spec §9.2: "no-op, error, hangs"). No row
 *  in `SLASH_COMMANDS` carries one of these today -- kept as real classes rather than folded away so
 *  a future row classed this way is held back with no further code change. */
const HELD_BACK_CLASSES: ReadonlySet<SlashCommandClass> = new Set(["no-op", "error", "hangs"]);

/** A draft's first word, if it is a slash command: `/name` alone, or `/name` followed by something
 *  else non-whitespace anywhere after it (an "argument" -- what tells `barePickerCommand` below a
 *  bare `/model`/`/effort` apart from `/model sonnet`). Returns `null` for a draft that is not a
 *  slash command at all -- ordinary text, sent as always. */
function parseSlashCommand(text: string): { name: string; hasArgument: boolean } | null {
  const trimmed = text.trimStart();
  if (!trimmed.startsWith("/")) return null;
  const match = /^\/(\S+)/.exec(trimmed);
  if (match === null) return null;
  const name = match[1];
  const rest = trimmed.slice(match[0].length);
  return { name, hasArgument: rest.trim().length > 0 };
}

/** Spec §9.2: whether Enter must refuse to send this draft. Returns the command name (without the
 *  leading "/", for the flash text below) when held back; `null` means send it as today -- a
 *  **works** or **sent-as-text** command, `/model`/`/effort` (bare or with an argument), and any
 *  other unknown `/word` all fall through to `null` here. */
export function heldBackSlashCommand(text: string): string | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null) return null;
  const { name } = parsed;
  if (INTERACTIVE_ONLY_UNCONDITIONAL.has(name)) return name;
  const entry = BY_NAME.get(name);
  if (entry !== undefined && HELD_BACK_CLASSES.has(entry.class)) return name;
  return null;
}

/** Owner trial item 2 (2026-09-28): whether `text` is exactly a bare `/model` or `/effort` -- no
 *  argument, nothing else typed -- the shape whose reply this panel now parses into a picker
 *  (`../components/SlashPicker`). Anything else (an argument present, a different command, plain
 *  text) is `null`: `App.tsx` arms the picker's "watch the next turn_completed" state only on this,
 *  never on `/model sonnet` (an ordinary, already-working send) or on the picker's own `onChoose`
 *  send, which always carries an argument and so is never mistaken for a fresh bare command. */
export function barePickerCommand(text: string): "model" | "effort" | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null || parsed.hasArgument) return null;
  return parsed.name === "model" || parsed.name === "effort" ? parsed.name : null;
}

/** Spec §9.2's own wording, with the product's name as it is now: "the band flashes `/<name> does not
 *  work in Eitri — ? lists the ones that do`". */
export function slashCommandFlashText(name: string): string {
  return `/${name} does not work in Eitri — ? lists the ones that do`;
}

/** Spec §9.2: "The `?` overlay gets a 'Slash commands' section listing the **works** row." --
 *  exactly the rows classed **works** in `SLASH_COMMANDS`' own order, `/model` and `/config`
 *  included even though both are held back from being sent (the table's class and the holdback
 *  above are deliberately independent, per the doc's own "Overlap with spec §9.2" note). */
export function worksSlashCommands(): string[] {
  return SLASH_COMMANDS.filter((entry) => entry.class === "works").map((entry) => entry.name);
}

/** What the `?` overlay lists (the v1-ui GUI pass, 2026-09-27): the **works** rows that Enter would
 *  actually send. The pass saw `/config` refused with that flash ("does not work in <the product> — ? lists
 *  the ones that do") while `?` listed `/config`, so the two contradicted each other on one screen. A command
 *  held back unconditionally (`/config`, the only one left since owner trial item 2 removed `/model`'s own
 *  holdback) is left out; `worksSlashCommands` above stays the table's own class, unchanged. */
export function listedSlashCommands(): string[] {
  return worksSlashCommands().flatMap((name) => (heldBackSlashCommand(`/${name}`) === null ? [name] : []));
}
