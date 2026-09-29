/** Parse the headless CLI reply to a bare `/model` or `/effort` into `SlashPicker` options. Read
 *  every option from the reply rather than maintaining a fixed list, so changed choices need no
 *  code change. Unrecognized reply shapes or empty option lists return `null`. `App.tsx` then
 *  displays the reply as ordinary text and opens no picker. */

/** A parsed `/model` reply: the options `/model <name>` accepts, in the CLI's own order, and the
 *  current model's name (the bare word inside the backticks after "Current model: ") when the
 *  reply names one. The probe's verbatim reply (CLI 2.1.283, the test-account wrapper):
 *  `"Current model: \`Haiku 4.5\` (effort: high)\nUsage: /model <name>. Available: sonnet, opus,
 *  haiku, fable, best, sonnet[1m], opus[1m], fable[1m], opusplan, default, or a full model ID."` */
export type ModelReply = { current: string | null; options: string[] };

const MODEL_CURRENT_RE = /Current model: `([^`]+)`/;
const MODEL_AVAILABLE_RE = /Available: (.+?), or a full model ID\./;

/** The "Current model: `Haiku 4.5`" line names the CLI's human-readable display name, not one of
 *  the short slugs `/model <name>` actually accepts (`options`, e.g. `"haiku"`) -- the two never
 *  match by `===`. Matched case-insensitively as a leading-word prefix (`"haiku 4.5".startsWith(
 *  "haiku")`) so `current`, once resolved, is always either `null` or literally one of `options` --
 *  the one property `SlashPicker`'s own `option === current` marking and cursor-seeding need, so
 *  neither has to re-derive this heuristic itself. The longest matching option wins, in case a
 *  future display name were a prefix of more than one slug (never observed on the probe's own
 *  reply, but cheap to get right). */
function matchCurrentOption(rawCurrent: string, options: string[]): string | null {
  const normalized = rawCurrent.toLowerCase();
  let best: string | null = null;
  for (const option of options) {
    if (normalized.startsWith(option.toLowerCase()) && (best === null || option.length > best.length)) best = option;
  }
  return best;
}

export function parseModelReply(text: string): ModelReply | null {
  const availableMatch = MODEL_AVAILABLE_RE.exec(text);
  if (availableMatch === null) return null;
  const options = availableMatch[1]
    .split(", ")
    .map((option) => option.trim())
    .filter((option) => option.length > 0);
  if (options.length === 0) return null;
  const currentMatch = MODEL_CURRENT_RE.exec(text);
  const current = currentMatch === null ? null : matchCurrentOption(currentMatch[1], options);
  return { current, options };
}

/** Levels accepted by `/effort <level>`, parsed from the bare command's reply. The usage reply
 *  lists options without a current level, so this type has no current selection for the picker
 *  to mark. */
export type EffortReply = { options: string[] };

const EFFORT_USAGE_RE = /Usage: \/effort <([^>]+)>/;

export function parseEffortReply(text: string): EffortReply | null {
  const match = EFFORT_USAGE_RE.exec(text);
  if (match === null) return null;
  const options = match[1]
    .split("|")
    .map((option) => option.trim())
    .filter((option) => option.length > 0);
  return options.length === 0 ? null : { options };
}
