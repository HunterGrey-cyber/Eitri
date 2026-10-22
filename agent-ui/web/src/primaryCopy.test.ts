// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import agentBridgeRs from "../../../core/src/agent_bridge.rs?raw";
import { copyToPrimary, PRIMARY_COPY_MAX_BYTES, utf8Length } from "./primaryCopy";

let posted: string[];

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(msg) } },
  };
});
afterEach(() => {
  delete (window as unknown as { webkit?: unknown }).webkit;
});

describe("copyToPrimary", () => {
  it("posts the copied text as it is, under the one kind the shell's page handler takes", () => {
    const text = "Run this:\n\nls -la\n\nthen look. é中\u{1f600}";
    copyToPrimary(text);
    expect(posted).toEqual([JSON.stringify({ type: "copy_primary", text })]);
  });

  it("sends a copy of exactly the cap and nothing for one byte more, counting UTF-8 as the shell does", () => {
    copyToPrimary("x".repeat(PRIMARY_COPY_MAX_BYTES));
    expect(posted).toHaveLength(1);
    copyToPrimary("x".repeat(PRIMARY_COPY_MAX_BYTES + 1));
    expect(posted).toHaveLength(1);
    // Two bytes each: half the cap in characters is the whole cap in bytes.
    copyToPrimary("é".repeat(PRIMARY_COPY_MAX_BYTES / 2));
    expect(posted).toHaveLength(2);
    copyToPrimary("é".repeat(PRIMARY_COPY_MAX_BYTES / 2) + "x");
    expect(posted).toHaveLength(2);
  });

  it("holds the same cap as the shell", () => {
    const rust = agentBridgeRs.match(/pub const PRIMARY_COPY_MAX_BYTES: usize = 1 << (\d+);/);
    expect(rust, "agent_bridge.rs names the cap as a power of two").not.toBeNull();
    expect(PRIMARY_COPY_MAX_BYTES).toBe(2 ** Number(rust![1]));
  });
});

describe("utf8Length", () => {
  it("counts what TextEncoder writes", () => {
    for (const text of ["", "abc", "é", "中文", "\u{1f600}", "a\u{1f600}bé中", "\n\t"]) {
      expect(utf8Length(text), JSON.stringify(text)).toBe(new TextEncoder().encode(text).length);
    }
  });

  it("counts a lone surrogate as the replacement character it becomes", () => {
    expect(utf8Length("\ud800")).toBe(3);
    expect(utf8Length("\udc00x")).toBe(4);
  });
});
