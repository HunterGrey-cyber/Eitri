import { describe, expect, it } from "vitest";
import { classify, failureEvidence, neverOpenedReason, sidecarStopped } from "./problems";
import watchRs from "../../../agent/src/providers/claude_sidecar/watch.rs?raw";
import agentPanelRs from "../../../shell/src/agent_panel.rs?raw";
import cliMissingFixture from "./fixtures/problems/cli-missing.txt?raw";
import cliOutOfRangeFixture from "./fixtures/problems/cli-out-of-range.txt?raw";
import notLoggedInFixture from "./fixtures/problems/not-logged-in.txt?raw";
import spawnRs from "../../../agent/src/providers/claude_sidecar/spawn.rs?raw";

describe("classify (spec §10.2)", () => {
  it("recognises the captured CLI-missing fixture", () => {
    const problem = classify(cliMissingFixture);
    expect(problem).toEqual({
      headline: "Claude Code (claude) was not found.",
      remedy: "Install it and make sure claude runs in a terminal, then press r.",
    });
  });

  it("recognises the captured CLI-out-of-range fixture and names the real versions", () => {
    const problem = classify(cliOutOfRangeFixture);
    expect(problem).toEqual({
      headline: "Claude Code 3.0.0 is outside the range this build supports (2.1.252 to below 3.0.0).",
      remedy: "Install a supported version, then press r.",
    });
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

  /* The v1-ui GUI pass (2026-09-27) captured what a logged-out account really produces: the
     sidecar's own account check refuses before binding its socket, so no turn is sent and the
     "Please run /login" string above never appears. Unrecognised, the failed tab showed only the
     raw text. */
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

  // Task 10 (`docs/canonical/2026-09-27-slash-commands.md`, "Step 3") tried to capture a real
  // not-logged-in fixture through the test-account wrapper and was refused: that wrapper hardcodes its own
  // config directory and discards any override, so there is no already-logged-in host this repo's
  // real-call tests can point at an empty one. What Task 10 DID capture, sending `/login` through a
  // normal (logged-in) session, is the same boilerplate every unsupported slash command gets --
  // `"/login isn't available in this environment."` -- which is not a login failure at all and must
  // NOT be classified as one; this pins that this classifier does not conflate the two. The real
  // shape a genuinely logged-out sidecar produces is still owed to the GUI pass (same doc, same
  // section) -- this classifier ships on Claude Code's own documented interactive string instead
  // (the two tests just above this one).
  it("does NOT classify /login's own captured 'isn't available in this environment' text as not-logged-in", () => {
    expect(classify("/login isn't available in this environment.")).toBeNull();
  });

  // Spec §10.2's last sentence: this diagnostic stays the header's own `⚠` and must NOT gain a
  // headline/remedy block here -- it is not a startup failure at all, and nothing above may match
  // it by accident (the real text has neither "refusing to start" nor "outside").
  it("returns null for the in-range-but-untested compatibility diagnostic", () => {
    const text =
      "claude CLI version 2.1.283 is inside this sidecar's supported range (>=2.1.252 <3.0.0) but " +
      "untested against it (tested: 2.1.267, 2.1.252, 2.1.270)";
    expect(classify(text)).toBeNull();
  });
});

/** v1 polish item 6: a resumed sidecar session whose sidecar stops before any new turn. The inner
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

  // And the wrapper against the `shell` code that writes it (`report_sessions_that_never_opened`,
  // via its `never_opened_message` helper): if its wording drifts, `failureEvidence` silently stops
  // unwrapping and the guess is shown again. Added by the local review of the cloud session's work
  // (2026-09-27).
  it("unwraps exactly what agent_panel.rs's never-opened format! writes", () => {
    const source = agentPanelRs.replace(/\\\n\s*/g, "");
    const literal = /"(the session ended before it started \(\{reason\}\)[^"]*)"/.exec(source);
    expect(literal).not.toBeNull();
    const written = literal![1].replace("{reason}", INNER);
    expect(neverOpenedReason(written)).toBe(INNER);
    expect(failureEvidence(written)).toBe(INNER);
  });

  // Owner decision (b), dated record 2026-09-27 ("v1 polish"): `never_opened_message` (shell) no
  // longer wraps a stopped-sidecar reason in the "most likely no longer exists" guess at all -- it
  // gets its own literal instead. Pinned the same way as the test above: read the literal out of
  // `agent_panel.rs`, write it with a real INNER, and check this module's classifier still reads it
  // as "the sidecar stopped" with no wrapper guess left over.
  it("shell's own never-opened-message literal for a stopped sidecar carries no guess", () => {
    const source = agentPanelRs.replace(/\\\n\s*/g, "");
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
