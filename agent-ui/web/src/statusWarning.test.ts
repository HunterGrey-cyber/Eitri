import { describe, expect, it } from "vitest";
import { statusWarning } from "./App";
import type { ProviderInfo } from "./types";

const provider = (diagnostics: string[]): ProviderInfo => ({
  sidecarVersion: "0.3.0",
  claudeAgentSdkVersion: "0.3.311",
  claudeCodeVersion: "2.1.320",
  protocol: "3.24",
  buildDescription: "packaged sidecar artifact: /usr/lib/eitri/verdandi-claude-sidecar",
  startupDiagnostics: diagnostics,
});

const untested = "claude-sidecar: host CLI version diagnostic: 2.1.320 is inside the supported range but untested";
const note = "If something looks wrong, a newer sidecar may fix it: eitri sidecar check";
const fallback =
  "Sidecar update 0.3.2 did not start (no answer in 15 s), so this tab runs the bundled sidecar 0.3.0. To stop trying it: eitri sidecar use bundled";

describe("statusWarning and a sidecar update", () => {
  it("draws nothing for an untested claude, its note included", () => {
    expect(statusWarning(provider([untested, note]), null)).toBeNull();
  });

  it("warns for a tab running on the fallback", () => {
    expect(statusWarning(provider([untested, note, fallback]), null)).toBe(fallback);
  });

  it("warns for the line about a selected update that cannot be used", () => {
    const line =
      "Sidecar update: the selected file does not name a version, so this tab runs the bundled sidecar 0.3.0. To stop trying it: eitri sidecar use bundled";
    expect(statusWarning(provider([line]), null)).toBe(line);
  });

  it("lets a failure win over the line", () => {
    expect(statusWarning(provider([fallback]), "failed")).toBe("failed");
  });
});
