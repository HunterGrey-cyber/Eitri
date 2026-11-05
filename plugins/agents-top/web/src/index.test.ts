import { describe, expect, it, vi } from "vitest";
import * as mod from "./index";
import { start } from "./index";
import { createStore } from "../../../../plugin-kit/agents/src/store";
import { bindKeys } from "../../../../plugin-kit/agents/src/keys";
import type { KeysKit } from "../../../../plugin-kit/agents/src/keys";
import { glyph, word } from "../../../../plugin-kit/agents/src/glyphs";

describe("package", () => {
  it("loads", () => {
    expect(mod).toBeDefined();
  });
});

it("sorting_preserves_the_shared_key_grammar_and_filter", async () => {
  const { store } = createStore();
  const bridge = { call: vi.fn(async () => ({})), on: () => () => {} };
  let installed: Parameters<KeysKit["install"]>[0] | undefined;
  const remove = vi.fn();
  const install: KeysKit["install"] = async options => { installed = options; return remove; };
  const keys = { install, message: vi.fn() };
  const agents = {
    version: 1 as const, store, glyph, word, start: async () => {},
    bindKeys: (options: Parameters<typeof bindKeys>[0]) => bindKeys(options, store, bridge, { capabilities: ["agents"] }),
  };
  const root = document.createElement("main");
  const oldKeys = window.eitriKeys;
  Object.assign(window, { eitriKeys: keys });
  try {
    const cleanup = await start(root, Object.assign(window, { eitriAgents: agents, eitriPlugin: bridge }));
    expect(keys.install).toBe(install);
    expect(installed!.rows.map(row => row.action)).toContain("agents.open");
    expect(installed!.rows.map(row => row.action)).toContain("top.reverse");
    const key = new KeyboardEvent("keydown");
    installed!.run("top.sort", key);
    expect(root.querySelector("button")?.textContent).toContain("age");
    installed!.run("top.reverse", key);
    expect(root.querySelector("button")?.textContent).toContain("▴");
    installed!.run("agents.filter", key);
    expect(document.querySelector('input[aria-label="Filter agents"]')).not.toBeNull();
    cleanup();
    expect(remove).toHaveBeenCalledOnce();
    expect(document.querySelector("input")).toBeNull();
  } finally {
    Object.assign(window, { eitriKeys: oldKeys });
  }
});
