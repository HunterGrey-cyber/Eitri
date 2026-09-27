/** Spec §10.2 (P11): a pure classifier from a failure text (the failed tab's `failure`, or an
 *  error row's raw text) to a headline and a remedy, drawn ABOVE that raw text, which always stays
 *  -- "never hide the evidence" (§10.2's own words). An unrecognised failure returns `null`, and
 *  callers must render exactly what they show today in that case: no headline, just the raw text.
 *
 *  Every recognizer here matches on a substring or shape of a real, captured diagnostic (the two
 *  fixtures under `fixtures/problems/`, §10.3) or, for "not logged in", on Claude Code's own
 *  documented interactive string -- see `docs/canonical/2026-09-27-slash-commands.md`'s "Step 3"
 *  for why that fixture was never captured. The sidecar's own "inside the supported range but
 *  untested" compatibility diagnostic is deliberately NOT matched by anything here: it stays the
 *  header's `⚠` (spec §10.2's last sentence), and this module has no rule that could catch it,
 *  which is the point -- adding one would be the regression. */

export type Problem = {
  headline: string;
  remedy: string;
};

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
  };
}

/** Captured verbatim in `fixtures/problems/cli-out-of-range.txt`. The sidecar's refusal names the
 *  host version once, then repeats it against the supported range as `>=A <B` -- captured so the
 *  headline can name the same two numbers the sidecar refused on, not a guess. Deliberately does
 *  NOT match the sidecar's other, non-fatal "inside the range but untested" diagnostic: that one
 *  never says "refusing to start" or "outside". */
const CLI_OUT_OF_RANGE_RE =
  /refusing to start: host claude CLI version ([^:]+): .*?is outside this sidecar's supported range \(>=(\S+) <([^)]+)\)/s;

function classifyCliOutOfRange(text: string): Problem | null {
  const m = CLI_OUT_OF_RANGE_RE.exec(text);
  if (m === null) return null;
  const [, version, low, high] = m;
  return {
    headline: `Claude Code ${version} is outside the range this build supports (${low} to below ${high}).`,
    remedy: "Install a supported version, then press r.",
  };
}

/** Never captured (`docs/canonical/2026-09-27-slash-commands.md`'s "Step 3": the test-account wrapper
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

/** Captured verbatim in `fixtures/problems/not-logged-in.txt` by the v1-ui GUI pass (2026-09-27): a
 *  sidecar started for an account whose config directory has no credentials (and no
 *  `ANTHROPIC_API_KEY`) refuses before it binds its socket -- Verdandi's own account check, ahead of
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
 *  connection to it could not be re-opened. The v1-ui walkthrough (item 6 of the v1 polish task)
 *  saw a resumed session whose sidecar died before any new turn read "it most likely no longer
 *  exists" (`neverOpenedReason` below) and lose its transcript, while the record was fine. */
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
 *  open (`report_sessions_that_never_opened`, shell/src/agent_panel.rs), split at its own words. A
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
 *  whole -- "never hide the evidence" (spec §10.2) means the reason, never the inference. */
export function failureEvidence(text: string): string {
  const inner = neverOpenedReason(text);
  return inner !== null && sidecarStopped(inner) ? inner : text;
}

/** Whether a problem's remedy already tells the reader to press `r`, so the row it heads leaves out
 *  its own "Press r to start a new session here." -- v1 polish item 7: a failed tab said it twice
 *  (three times with the composer's line, which no longer repeats it). */
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

/** `account` is only consulted by the "not logged in" row (spec §10.2's own "plus, with
 *  `--account`, the account's config directory"); every other caller may omit it. */
export function classify(text: string, account: string | null = null): Problem | null {
  for (const classifier of CLASSIFIERS) {
    const problem = classifier(text, account);
    if (problem !== null) return problem;
  }
  return null;
}
