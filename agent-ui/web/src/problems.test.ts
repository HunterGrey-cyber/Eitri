import { describe, expect, it } from "vitest";
import { classify } from "./problems";
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
      "  - /home/user/.local/lib/neovibe/verdandi-claude-sidecar\n" +
      "Install the neovibe package that ships it (verdandi-claude-sidecar), or point NEOVIBE_SIDECAR_BINARY at a real one.";
    const problem = classify(text);
    expect(problem).toEqual({
      headline: "The agent sidecar is not installed.",
      remedy:
        "Looked for it at:\n" +
        "  - /home/user/.local/lib/neovibe/verdandi-claude-sidecar\n" +
        "Install the neovibe package that ships it (verdandi-claude-sidecar), or point NEOVIBE_SIDECAR_BINARY at a real one.",
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

  // An unsupported `/login` reply says the command is unavailable even in a logged-in session.
  // That text must not be classified as a login failure. The login recognizer uses the interactive
  // authentication diagnostic; a logged-out sidecar fixture has not been captured.
  it("does NOT classify /login's own captured 'isn't available in this environment' text as not-logged-in", () => {
    expect(classify("/login isn't available in this environment.")).toBeNull();
  });

  // The in-range-but-untested diagnostic stays in the header as `⚠`. It is not a startup failure
  // and must not gain a headline or remedy here. Its text contains neither "refusing to start"
  // nor "outside", so the failure recognizers must leave it unmatched.
  it("returns null for the in-range-but-untested compatibility diagnostic", () => {
    const text =
      "claude CLI version 2.1.283 is inside this sidecar's supported range (>=2.1.252 <3.0.0) but " +
      "untested against it (tested: 2.1.267, 2.1.252, 2.1.270)";
    expect(classify(text)).toBeNull();
  });
});
