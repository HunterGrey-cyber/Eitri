import { describe, expect, it } from "vitest";
import { statusRowText, statusWarning } from "./statusRow";
import { initialState } from "./reducer";
import type { ProviderInfo } from "./types";

const provider = (diagnostics: string[]): ProviderInfo => ({
  sidecarVersion: "0.9",
  claudeAgentSdkVersion: "0.2",
  claudeCodeVersion: "2.1.282",
  protocol: "3.4",
  buildDescription: "Verdandi checkout: /v @ 28a5e4c (via default path)",
  startupDiagnostics: diagnostics,
});

describe("statusWarning (D12 A, ruling 9)", () => {
  it("warns for Verdandi skew", () => {
    const drift = "Verdandi baseline drift: running 1234567, this client was verified against 28a5e4c.";
    expect(statusWarning(provider([drift]), null)).toBe(drift);
  });
  it("warns for a refusal, which is a tab that failed to start", () => {
    expect(statusWarning(null, "the CLI gate refused 2.1.999")).toBe("the CLI gate refused 2.1.999");
  });
  it("does not warn for the routine CLI version diagnostic, which goes to prefix i only", () => {
    expect(
      statusWarning(provider(["[claude-sidecar] CLI version diagnostic: 2.1.282 is in range but untested"]), null),
    ).toBeNull();
    expect(statusWarning(provider([]), null)).toBeNull();
    expect(statusWarning(null, null)).toBeNull();
  });
});

describe("statusRowText", () => {
  it("is model · backend · position", () => {
    const state = { ...initialState(), model: "claude-sonnet-5", backend: "sidecar" as const };
    expect(statusRowText(state, { index: 1, total: 12 })).toBe("claude-sonnet-5 · sidecar · 2/12");
    expect(statusRowText({ ...state, model: null }, { index: 0, total: 0 })).toBe("no model yet · sidecar · —");
  });
});
