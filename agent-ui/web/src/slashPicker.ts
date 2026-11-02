import { shortModel } from "./band";
import type { AppliedSettings, EffortChoice, ModelOption } from "./types";

/** Parses the headless CLI's own reply to a bare `/model` or `/effort` into the
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
 *  the picker this builds; the probe's own recommendation says so in the
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

/** A picker opened from the session's own list rather than from a slash command's reply. `labels` is what each
 *  row reads, `current` the row the session runs on. */
export type DirectPicker = { options: string[]; current: string | null; labels: Record<string, string> };

/** The model picker from the CLI's list, in its order. `current` is the row whose id is the one the CLI says it
 *  will use: the named row when one is, since `default` resolves to the same id as the row it stands for. A model
 *  that cannot run auto says so on its row, because choosing it ends auto for the session. */
export function modelPicker(models: ModelOption[], applied: AppliedSettings, agentLists = false): DirectPicker | null {
  if (models.length === 0) return null;
  const running = models.filter((row) => runsOn(row, applied));
  const current = running.find((row) => row.value !== "default") ?? running[0];
  return {
    options: models.map((row) => row.value),
    current: current?.value ?? null,
    // An agent that lists its own choices has no auto mode of ours to name.
    labels: Object.fromEntries(models.map((row) => [row.value, agentLists ? agentRowWords(row.value, row.displayName) : modelRowWords(row)])),
  };
}

/** A row of an agent's own list: its name for the value, and the value itself when that is all there is. */
function agentRowWords(value: string, name: string): string {
  return name === "" || name === value ? value : `${name} (${value})`;
}

/** Whether the session runs on this row. An empty `applied.model` is a read that did not answer, and names no row. */
function runsOn(row: ModelOption, applied: AppliedSettings): boolean {
  return applied.model !== "" && (row.value === applied.model || row.resolvedModel === applied.model);
}

function modelRowWords(row: ModelOption): string {
  const name = row.displayName === "" ? row.value : row.displayName;
  return row.supportsAutoMode ? name : `${name} (no auto mode)`;
}

/** The level that goes back to what the settings say, as the CLI's own `/effort` names it. */
export const EFFORT_FROM_SETTINGS = "auto";

/** The effort picker for the model the session runs on: `null` when that model takes no level. A list that does
 *  not name the model offers the levels the rows have in common with it, the union of the list. */
export function effortPicker(
  models: ModelOption[],
  applied: AppliedSettings,
  agentEfforts: EffortChoice[] | null = null,
): DirectPicker | null {
  if (agentEfforts !== null) {
    // An agent lists its levels itself; there is no setting to go back to.
    if (agentEfforts.length === 0) return null;
    const values = agentEfforts.map((level) => level.value);
    return {
      options: values,
      current: values.includes(applied.effort) ? applied.effort : null,
      labels: Object.fromEntries(agentEfforts.map((level) => [level.value, agentRowWords(level.value, level.displayName)])),
    };
  }
  const running = models.find((row) => runsOn(row, applied));
  if (running !== undefined && !running.supportsEffort) return null;
  const levels = running?.supportedEffortLevels ?? [...new Set(models.flatMap((row) => row.supportedEffortLevels))];
  if (levels.length === 0) return null;
  const options = [...levels, EFFORT_FROM_SETTINGS];
  return { options, current: levels.includes(applied.effort) ? applied.effort : null, labels: { [EFFORT_FROM_SETTINGS]: "as the settings say" } };
}

/** What the band says once a change is made. A model changes at the next request, so a running response finishes on
 *  the model it started with, and the rest of its turn uses the new one. */
export function settingsChangedWords(
  kind: "model" | "effort",
  applied: AppliedSettings,
  chosen: string,
  running: { model: string | null } | null,
  agentLists = false,
): string {
  // The agent says what it now runs; when it takes the change is its own matter.
  if (agentLists) return kind === "effort" ? `effort: ${applied.effort === "" ? chosen : applied.effort}` : `model: ${applied.model === "" ? chosen : applied.model}`;
  if (kind === "effort") {
    // A read that answered always names the model, so an empty model is no answer, and says nothing of the level.
    const level = applied.model === "" ? chosen : applied.effort === "" ? "none on this model" : applied.effort;
    return `effort: ${level}, from the next request`;
  }
  const model = applied.model === "" ? chosen : applied.model;
  const word = shortModel(model);
  if (running === null) return `model: ${word}, from the next turn`;
  return running.model === null || running.model === model
    ? `model: ${word}, for the rest of this turn`
    : `model: ${word}; the running response finishes on ${shortModel(running.model)}`;
}
