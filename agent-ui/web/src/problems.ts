/** A pure classifier from a failure text (the failed tab's `failure`, or an
 *  error row's raw text) to a headline and a remedy, drawn ABOVE that raw text, which always stays
 *  -- the evidence is never hidden. An unrecognised failure returns `null`, and
 *  callers must render exactly what they show today in that case: no headline, just the raw text.
 *
 *  Every recognizer here matches on a substring or shape of a real, captured diagnostic (the three
 *  fixtures under `fixtures/problems/`) or, for "not logged in", on Claude Code's own
 *  documented interactive string, since the CLI's own logged-out diagnostic was never captured (the
 *  sidecar's refusal of a logged-out account is a fixture). The sidecar's own "inside the supported range but
 *  untested" compatibility diagnostic is deliberately NOT matched by anything here: it appears in
 *  the detail popover's CLI row and the window's stderr. Where the sidecar command exists, Rust adds
 *  its static hint there and to a failed tab's raw text, which is always drawn. */

export type Problem = {
  headline: string;
  remedy: string;
  /** A web address the reader is shown under the remedy, drawn as a link. */
  link?: string;
};

/** Where Claude Code's install instructions are; the same address `agent::CLAUDE_INSTALL_URL` gives the
 *  first-start screen and `packaging/install.sh` gives its warning (a test reads all three). */
export const CLAUDE_INSTALL_URL = "https://code.claude.com/docs/en/quickstart";

/** `agent::sidecar_missing_message`'s exact opening sentence (agent/src/providers/claude_sidecar/
 *  spawn.rs). Recognising it literally, rather than re-deriving it, is deliberate: this text is
 *  produced by this project's own Rust, not by the sidecar or the CLI, so there is nothing to
 *  parse -- only to recognise and split into headline / remedy. */
const SIDECAR_MISSING_HEADLINE = "The agent sidecar is not installed.";

function classifySidecarMissing(text: string): Problem | null {
  const at = text.indexOf(SIDECAR_MISSING_HEADLINE);
  if (at === -1) return null;
  const remedy = text.slice(at + SIDECAR_MISSING_HEADLINE.length).trim();
  return { headline: SIDECAR_MISSING_HEADLINE, remedy };
}

/** Captured verbatim in `fixtures/problems/cli-missing.txt`: the sidecar's own
 *  `cliCompatibility.ts` says this when `spawnSync`ing the configured CLI path fails outright
 *  (ENOENT), before any version parsing is attempted. */
const CLI_MISSING_MARKER = "could not determine the installed claude CLI version";

function classifyCliMissing(text: string): Problem | null {
  if (!text.includes(CLI_MISSING_MARKER)) return null;
  return {
    headline: "Claude Code (claude) was not found.",
    remedy: "Install it and make sure claude runs in a terminal, then press r.",
    link: CLAUDE_INSTALL_URL,
  };
}

/** Captured verbatim in `fixtures/problems/cli-out-of-range.txt`. The sidecar's refusal names the
 *  host version once, then repeats it against the supported range as `>=A <B` -- captured so the
 *  headline can name the same two numbers the sidecar refused on, not a guess. Deliberately does
 *  NOT match the sidecar's other, non-fatal "inside the range but untested" diagnostic: that one
 *  never says "refusing to start" or "outside". */
const CLI_OUT_OF_RANGE_RE =
  /refusing to start: host claude CLI version ([^:]+): .*?is outside this sidecar's supported range \(>=(\S+) <([^)]+)\)/s;

/** The sentence `agent::REFUSED_CLAUDE_NOTE` adds where the `eitri sidecar` command exists and a newer sidecar could
 *  help; only the command's name is looked for, so the sentence can be reworded in Rust. */
const SIDECAR_CHECK_COMMAND = "eitri sidecar check";

/** `a` against `b`, as dotted numbers: negative when `a` is the older. A part that is not a number counts as 0. */
function compareVersions(a: string, b: string): number {
  const left = a.split(".").map((part) => parseInt(part, 10) || 0);
  const right = b.split(".").map((part) => parseInt(part, 10) || 0);
  for (let i = 0; i < Math.max(left.length, right.length); i += 1) {
    const difference = (left[i] ?? 0) - (right[i] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

function classifyCliOutOfRange(text: string): Problem | null {
  const m = CLI_OUT_OF_RANGE_RE.exec(text);
  if (m === null) return null;
  const [, version, low, high] = m;
  const works = `${low} up to, but not including, ${high}`;
  // Below the floor, a newer sidecar only raises the floor: the way out is a newer Claude Code. Above the ceiling a
  // newer sidecar may be the fix, and only where the command exists does the page say so.
  let remedy: string;
  if (compareVersions(version, low) < 0) {
    remedy = `Update Claude Code: this build works with ${works}. Then press r.`;
  } else if (text.includes(SIDECAR_CHECK_COMMAND)) {
    remedy = `A newer sidecar may support it: run ${SIDECAR_CHECK_COMMAND} in a terminal. Otherwise install a Claude Code this build works with (${works}), then press r.`;
  } else {
    remedy = `Install a Claude Code this build works with (${works}), then press r.`;
  }
  return {
    headline: `Claude Code ${version} is outside the range this build supports (${low} to below ${high}).`,
    remedy,
  };
}

/** Never captured (the test-account wrapper
 *  refuses to point at an empty scratch config directory, so making it appear logged out was never
 *  attempted). Ships on Claude Code's own documented interactive string instead. */
const NOT_LOGGED_IN_MARKER = "Please run /login";

function classifyNotLoggedIn(text: string, account: string | null): Problem | null {
  if (!text.includes(NOT_LOGGED_IN_MARKER)) return null;
  const remedy =
    account === null
      ? "Run claude in a terminal and /login, then send again."
      : `Run claude in a terminal and /login, then send again. (account "${account}": ~/.claude-${account})`;
  return { headline: "Claude Code is not logged in.", remedy };
}

/** `fixtures/problems/not-logged-in.txt` is a real capture of what a sidecar started for an account
 *  whose config directory has no credentials (and no `ANTHROPIC_API_KEY`) prints when it refuses
 *  before binding its socket -- Verdandi's own account check, ahead of
 *  the CLI version check, so no turn is ever sent and "Please run /login" never appears. Names the
 *  account and its directory from the text itself, which already carries both (a
 *  `VERDANDI_CLAUDE_CONFIG_DIR` override included, which `~/.claude-<name>` would get wrong). */
const SIDECAR_NOT_LOGGED_IN_RE = /claude account "([^"]+)" is not logged in: (.+?)\/\.credentials\.json is missing/;

function classifySidecarNotLoggedIn(text: string): Problem | null {
  const m = SIDECAR_NOT_LOGGED_IN_RE.exec(text);
  if (m === null) return null;
  const [, account, dir] = m;
  return {
    headline: "Claude Code is not logged in.",
    remedy: `Run claude in a terminal and /login, then press r. (account "${account}": ${dir})`,
  };
}

/** `stream_ended_early_reason`'s own opening (agent/src/providers/claude_sidecar/watch.rs): the
 *  sidecar's event stream ended before the session did -- the sidecar process stopped, or the
 *  connection to it could not be re-opened. If it stops before a resumed session
 *  sends its first turn, the wrapper's "it most likely no longer exists" guess is wrong and loses its
 *  transcript while the record can still be fine (`neverOpenedReason` below). */
const SIDECAR_STOPPED_MARKER = "the connection to the provider ended before this session did";

/** Whether `text` says the sidecar stopped under a session (see `SIDECAR_STOPPED_MARKER`). */
export function sidecarStopped(text: string): boolean {
  return text.includes(SIDECAR_STOPPED_MARKER);
}

function classifySidecarStopped(text: string): Problem | null {
  if (!sidecarStopped(text)) return null;
  return {
    headline: "The agent sidecar stopped.",
    remedy: "The conversation is still in the session list: press r, then w, and pick it to continue it.",
  };
}

/** `shell`'s wrapper for a session that reached a terminal state before the provider reported it
 *  open (`report_sessions_that_never_opened`, panel/src/starts.rs), split at its own words. A
 *  resumed sidecar session reports itself open only at its first turn, so a sidecar that stops
 *  before one lands here too, and the wrapper's guess ("it most likely no longer exists") is then
 *  wrong. Returns the provider's own reason when `text` is exactly that wrapper, else `null`. */
const NEVER_OPENED_HEAD = "the session ended before it started (";
const NEVER_OPENED_TAIL =
  "). If you were continuing a previous conversation, it most likely no longer exists -- start a new session instead.";

export function neverOpenedReason(text: string): string | null {
  if (!text.startsWith(NEVER_OPENED_HEAD) || !text.endsWith(NEVER_OPENED_TAIL)) return null;
  return text.slice(NEVER_OPENED_HEAD.length, text.length - NEVER_OPENED_TAIL.length);
}

/** The text a failure row shows under its headline: the provider's own reason when `shell`
 *  wrapped it in a guess this module knows to be wrong (a sidecar that stopped), else `text`
 *  whole -- "never hide the evidence" means the reason, never the inference. */
export function failureEvidence(text: string): string {
  const inner = neverOpenedReason(text);
  return inner !== null && sidecarStopped(inner) ? inner : text;
}

/** Whether a problem's remedy already tells the reader to press `r`, so the row it heads leaves out
 *  its own "Press r to start a new session here." to avoid repeating the remedy. */
export function remedyNamesR(problem: Problem | null): boolean {
  return problem !== null && /\bpress r\b/i.test(problem.remedy);
}

/** Order matters only in that each recognizer's own marker is specific enough that at most one
 *  ever matches a real diagnostic; there is no priority contest to get right. */
const CLASSIFIERS: ((text: string, account: string | null) => Problem | null)[] = [
  (text) => classifySidecarMissing(text),
  (text) => classifyCliMissing(text),
  (text) => classifyCliOutOfRange(text),
  (text) => classifySidecarNotLoggedIn(text),
  (text) => classifySidecarStopped(text),
  (text, account) => classifyNotLoggedIn(text, account),
];

/** `account` is only consulted by the "not logged in" row (which, with `--account`, also names the
 *  account's config directory); every other caller may omit it. */
export function classify(text: string, account: string | null = null): Problem | null {
  for (const classifier of CLASSIFIERS) {
    const problem = classifier(text, account);
    if (problem !== null) return problem;
  }
  return null;
}
