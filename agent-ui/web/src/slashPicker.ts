/** Owner trial item 2 (2026-09-28, `the private review notes` §2 and its
 *  same-day probe): parses the headless CLI's own reply to a bare `/model` or `/effort` into the
 *  options `../components/SlashPicker` shows. Deliberately never a hard-coded model/effort list --
 *  every option comes out of the reply text itself, so a CLI whose choices change is reflected here
 *  with no code change, and a CLI whose reply shape changed entirely (a version this was never
 *  checked against) simply fails to parse, which both functions report as `null` rather than a
 *  guess. `App.tsx` shows the reply as ordinary text and opens no picker on `null` -- never a
 *  picker with nothing in it. */

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

/** A parsed `/effort` reply: the levels `/effort <level>` accepts. The bare command's own reply
 *  never names a current level (unlike `/model`'s) -- CLI 2.1.283's verbatim reply is only
 *  `"Usage: /effort <low|medium|high|xhigh|max|auto>"` -- so there is nothing to mark as current in
 *  the picker this builds; the probe's own recommendation (owner trial item 2) says so in the
 *  picker's hint line instead. */
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
