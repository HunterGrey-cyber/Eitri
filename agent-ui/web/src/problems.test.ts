import { describe, expect, it } from "vitest";
import { CLAUDE_INSTALL_URL, classify, failureEvidence, neverOpenedReason, sidecarStopped } from "./problems";
import cliLocateRs from "../../../agent/src/providers/claude_sidecar/cli_locate.rs?raw";
import installSh from "../../../packaging/install.sh?raw";
import watchRs from "../../../agent/src/providers/claude_sidecar/watch.rs?raw";
import startsRs from "../../../panel/src/starts.rs?raw";
import cliMissingFixture from "./fixtures/problems/cli-missing.txt?raw";
import cliOutOfRangeFixture from "./fixtures/problems/cli-out-of-range.txt?raw";
import notLoggedInFixture from "./fixtures/problems/not-logged-in.txt?raw";
import spawnRs from "../../../agent/src/providers/claude_sidecar/spawn.rs?raw";

describe("classify", () => {
  it("recognises the captured CLI-missing fixture", () => {
    const problem = classify(cliMissingFixture);
    expect(problem).toEqual({
      headline: "Claude Code (claude) was not found.",
      remedy: "Install it and make sure claude runs in a terminal, then press r.",
      link: "https://code.claude.com/docs/en/quickstart",
    });
  });

  /* One address, given in four places: the panel's row (here), the first-start screen (`agent::CLAUDE_INSTALL_URL`),
     the installer's warning, and the docs. A change to one that misses another fails here. */
  it("gives the same install address as the Rust constant and the installer's warning", () => {
    expect(cliLocateRs).toContain(`pub const CLAUDE_INSTALL_URL: &str = "${CLAUDE_INSTALL_URL}";`);
    expect(installSh).toContain(`the claude CLI was not found on PATH`);
    expect(installSh).toContain(CLAUDE_INSTALL_URL);
  });

  /* The page recognises a not-found failure by the sidecar's words; Rust holds `r` on the same words until a lookup
     finds `claude`, so the two spell them alike. */
  it("spells the not-found failure marker as the Rust constant does", () => {
    const marker = /pub const CLI_MISSING_FAILURE_MARKER: &str = "([^"]+)";/.exec(cliLocateRs)![1];
    expect(cliMissingFixture).toContain(marker);
    expect(classify(`${marker} via "/x"`)?.headline).toBe("Claude Code (claude) was not found.");
  });

  it("recognises the captured CLI-out-of-range fixture and names the real versions", () => {
    const problem = classify(cliOutOfRangeFixture);
    expect(problem).toEqual({
      headline: "Claude Code 3.0.0 is outside the range this build supports (2.1.252 to below 3.0.0).",
      remedy: "Install a Claude Code this build works with (2.1.252 up to, but not including, 3.0.0), then press r.",
    });
  });

  it("points an out-of-range Claude Code at eitri sidecar check only where the note says the command exists", () => {
    const withNote = `${cliOutOfRangeFixture}\nA newer sidecar may support this claude: eitri sidecar check`;
    expect(classify(withNote)?.remedy).toBe(
      "A newer sidecar may support it: run eitri sidecar check in a terminal. Otherwise install a Claude Code this build works with (2.1.252 up to, but not including, 3.0.0), then press r.",
    );
  });

  it("sends a Claude Code below the floor to a newer Claude Code, never to a sidecar", () => {
    const below = cliOutOfRangeFixture.replace("3.0.0: claude CLI version 3.0.0", "2.1.200: claude CLI version 2.1.200");
    const withNote = `${below}\nA newer sidecar may support this claude: eitri sidecar check`;
    const problem = classify(withNote)!;
    expect(problem.headline).toBe("Claude Code 2.1.200 is outside the range this build supports (2.1.252 to below 3.0.0).");
    expect(problem.remedy).toBe(
      "Update Claude Code: this build works with 2.1.252 up to, but not including, 3.0.0. Then press r.",
    );
  });

  it("never tells a person to widen the range in the sidecar's source", () => {
    for (const text of [cliOutOfRangeFixture]) {
      const problem = classify(text)!;
      expect(`${problem.headline} ${problem.remedy}`).not.toMatch(/cliCompatibility|widen/);
    }
  });

  it("recognises agent::sidecar_missing_message's output and splits it into headline and remedy", () => {
    // The literal shape `agent::sidecar_missing_message` produces (agent/src/providers/
    // claude_sidecar/spawn.rs) -- not captured as a fixture file, since this text is this
    // project's own Rust rather than something the sidecar or CLI printed.
    const text =
      "The agent sidecar is not installed. Looked for it at:\n" +
      "  - /home/user/.local/lib/eitri/verdandi-claude-sidecar\n" +
      "Install the eitri package that ships it (verdandi-claude-sidecar), or point EITRI_SIDECAR_BINARY at a real one.";
    const problem = classify(text);
    expect(problem).toEqual({
      headline: "The agent sidecar is not installed.",
      remedy:
        "Looked for it at:\n" +
        "  - /home/user/.local/lib/eitri/verdandi-claude-sidecar\n" +
        "Install the eitri package that ships it (verdandi-claude-sidecar), or point EITRI_SIDECAR_BINARY at a real one.",
    });
  });

  // Pins the literal this test's own fixture text depends on: if Rust's opening sentence ever
  // changes, this fails here rather than only in a Rust unit test nobody thought to cross-check.
  it("agent::sidecar_missing_message's opening sentence, as pinned in Rust, matches this module's marker", () => {
    expect(spawnRs).toContain('"The agent sidecar is not installed.');
  });

  it("ships 'not logged in' on Claude Code's own documented string, never captured as a fixture", () => {
    const text = "Invalid API key · Please run /login";
    expect(classify(text)).toEqual({
      headline: "Claude Code is not logged in.",
      remedy: "Run claude in a terminal and /login, then send again.",
    });
  });

  it("names the account's config directory when one is configured", () => {
    const text = "Invalid API key · Please run /login";
    expect(classify(text, "work")).toEqual({
      headline: "Claude Code is not logged in.",
      remedy: 'Run claude in a terminal and /login, then send again. (account "work": ~/.claude-work)',
    });
  });

  /* A logged-out account makes the sidecar's own account check refuse before its socket binds, so
     no turn is sent and the "Please run /login" string above never appears. Unrecognised, the
     failed tab showed only the raw text. */
  it("recognises the captured logged-out sidecar and names the account and its directory", () => {
    expect(classify(notLoggedInFixture)).toEqual({
      headline: "Claude Code is not logged in.",
      remedy: 'Run claude in a terminal and /login, then press r. (account "scratch": /scratch/v1ui-gui/claude-scratch)',
    });
  });

  it("recognises it inside the failed tab's own prefix too", () => {
    expect(classify(`failed to connect to the Verdandi sidecar: ${notLoggedInFixture}`, "other")?.headline).toBe(
      "Claude Code is not logged in.",
    );
  });

  it("returns null for a failure it does not recognise", () => {
    expect(classify("claude-sidecar exited with signal: 11 (SIGSEGV); its stderr said:\nsegfault")).toBeNull();
  });

  // the test-account wrapper hardcodes its own config directory and discards overrides, so a real-call
  // test cannot point an already-logged-in host at an empty one. Sending `/login` through a normal
  // logged-in session yields the same boilerplate as every unsupported slash command --
  // `"/login isn't available in this environment."` -- which is not a login failure and must not be
  // classified as one. This pins that the classifier keeps those cases distinct; the logged-out
  // sidecar message follows Claude Code's documented interactive string instead.
  // (the two tests just above this one).
  it("does NOT classify /login's own captured 'isn't available in this environment' text as not-logged-in", () => {
    expect(classify("/login isn't available in this environment.")).toBeNull();
  });

  // This diagnostic stays the header's own `⚠` and must NOT gain a
  // headline/remedy block here -- it is not a startup failure at all, and nothing above may match
  // it by accident (the real text has neither "refusing to start" nor "outside").
  it("returns null for the in-range-but-untested compatibility diagnostic", () => {
    const text =
      "claude CLI version 2.1.283 is inside this sidecar's supported range (>=2.1.252 <3.0.0) but " +
      "untested against it (tested: 2.1.267, 2.1.252, 2.1.270)";
    expect(classify(text)).toBeNull();
  });
});

/** A resumed sidecar session whose sidecar stops before any new turn. The inner
 *  reason is `stream_ended_early_reason`'s; the wrapper is `shell`'s "never opened" guess. */
describe("a sidecar that stopped under a session", () => {
  const INNER =
    "the connection to the provider ended before this session did, so anything after this point never arrived and the reply above may be incomplete (the provider closed the event stream)";
  const WRAPPED = `the session ended before it started (${INNER}). If you were continuing a previous conversation, it most likely no longer exists -- start a new session instead.`;

  it("is classified as the sidecar stopping, wrapped or not, and says the record is kept", () => {
    for (const text of [INNER, WRAPPED]) {
      expect(sidecarStopped(text)).toBe(true);
      const problem = classify(text)!;
      expect(problem.headline).toBe("The agent sidecar stopped.");
      expect(problem.remedy).toContain("session list");
    }
  });

  it("shows the provider's own reason, not the wrapper's guess", () => {
    expect(neverOpenedReason(WRAPPED)).toBe(INNER);
    expect(failureEvidence(WRAPPED)).toBe(INNER);
    expect(failureEvidence(INNER)).toBe(INNER);
    // Any other never-opened reason keeps the whole text: only a stopped sidecar is known to make
    // the guess wrong.
    const other = "the session ended before it started (claude exited 1). If you were continuing a previous conversation, it most likely no longer exists -- start a new session instead.";
    expect(failureEvidence(other)).toBe(other);
    expect(classify(other)).toBeNull();
  });

  // Pins the marker against the Rust that writes it, the same way the sidecar-missing test above does.
  it("matches watch.rs's own wording", () => {
    expect(watchRs.replace(/\s+\\?\n\s*/g, " ")).toContain("the connection to the provider ended before this session did");
  });

  // And the wrapper against the panel's Rust that writes it (`report_sessions_that_never_opened`,
  // via its `never_opened_message` helper): if its wording drifts, `failureEvidence` silently stops
  // unwrapping and the guess is shown again.
  it("unwraps exactly what starts.rs's never-opened format! writes", () => {
    const source = startsRs.replace(/\\\n\s*/g, "");
    const literal = /"(the session ended before it started \(\{reason\}\)[^"]*)"/.exec(source);
    expect(literal).not.toBeNull();
    const written = literal![1].replace("{reason}", INNER);
    expect(neverOpenedReason(written)).toBe(INNER);
    expect(failureEvidence(written)).toBe(INNER);
  });

  // `never_opened_message` gives a stopped sidecar its own literal rather than wrapping it in the
  // "most likely no longer exists" guess. Read that literal from `starts.rs`, write it with a
  // real INNER, and check this module still identifies it as "the sidecar stopped" with no wrapper
  // guess left over.
  it("shell's own never-opened-message literal for a stopped sidecar carries no guess", () => {
    const source = startsRs.replace(/\\\n\s*/g, "");
    const literal = /"(the agent sidecar stopped \(\{reason\}\)[^"]*)"/.exec(source);
    expect(literal).not.toBeNull();
    const written = literal![1].replace("{reason}", INNER);
    // Not the wrapped shape any more, so neverOpenedReason must NOT unwrap it -- there is nothing
    // to unwrap: failureEvidence returns it whole, exactly as `never hide the evidence` requires.
    expect(neverOpenedReason(written)).toBeNull();
    expect(failureEvidence(written)).toBe(written);
    expect(written).not.toContain("most likely no longer exists");
    const problem = classify(written)!;
    expect(problem.headline).toBe("The agent sidecar stopped.");
  });
});
