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

/** Order matters only in that each recognizer's own marker is specific enough that at most one
 *  ever matches a real diagnostic; there is no priority contest to get right. */
const CLASSIFIERS: ((text: string, account: string | null) => Problem | null)[] = [
  (text) => classifySidecarMissing(text),
  (text) => classifyCliMissing(text),
  (text) => classifyCliOutOfRange(text),
  (text) => classifySidecarNotLoggedIn(text),
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
