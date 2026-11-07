// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

afterEach(cleanup);
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });

const raw = JSON.stringify({ path: "/p/a.py", modified: true, first_line: 4, last_line: 5, text: "one\ntwo\n" });

function mount(name = "mcp__eitri__read_buffer", input: unknown = { path: "a.py" }, content: unknown = raw, isError = false) {
  const posted: unknown[] = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (message: string) => posted.push(JSON.parse(message)) } },
  };
  const { container } = render(<App />);
  const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"],
    resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
    marker: null, pending: 0, resumable: false, failure: null }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" },
    toolCalls: [{ seq: 1, toolUseId: "read", name, input, result: { content, isError } }], nextSeq: 2 } });
  const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string, ctrlKey = false) => fireEvent.keyDown(root, { key, ctrlKey });
  press("Escape");
  press("g"); press("g");
  const body = () => container.querySelector(".tool-result-body");
  return { container, press, body, posted };
}

describe("editor tool expanded answers in App", () => {
  it.each(["Enter", "za", "zo"])("%s expands to readable buffer text", (key) => {
    const { body, press } = mount();
    expect(body()).toBeNull();
    for (const part of key === "Enter" ? [key] : [...key]) press(part);
    expect(body()?.textContent).toBe("/p/a.py:4-5 (unsaved)\none\ntwo\n");
    expect(body()?.textContent).not.toContain('{"');
  });

  it("folded, the preview shows the readable text's first lines, not the JSON (the CLI's MCP block shape)", () => {
    const { container } = mount("mcp__eitri__read_buffer", { path: "a.py" }, [{ type: "text", text: raw }]);
    const preview = container.querySelector(".tool-result-preview");
    expect(preview?.textContent).toContain("/p/a.py:4-5 (unsaved)");
    expect(preview?.textContent).not.toContain('"text"');
    expect(preview?.textContent).not.toContain("{");
  });

  it("Ctrl+o shows the raw answer, and returning restores the readable body", () => {
    const { body, press } = mount();
    press("Enter");
    press("o", true);
    expect(body()?.textContent).toBe(raw);
    press("o", true);
    expect(body()?.textContent).toBe("/p/a.py:4-5 (unsaved)\none\ntwo\n");
  });

  it("uses the same readable body for Codex's eitri MCP call", () => {
    const { body, press } = mount("mcpToolCall", { server: "eitri", tool: "read_buffer", arguments: { path: "a.py" } },
      { content: [{ type: "text", text: raw }] });
    press("Enter");
    expect(body()?.textContent).toBe("/p/a.py:4-5 (unsaved)\none\ntwo\n");
    press("o", true);
    expect(body()?.textContent).toContain('"type": "text"');
    expect(body()?.textContent).toContain('\\"modified\\":true');
  });

  it("shows hover markup as text and reveals hidden characters in both views", () => {
    const text = "**hover** <img src=x> [link](https://example.test)\u202e\u200b";
    const { body, press } = mount("mcp__eitri__hover", { path: "a.py", symbol: "foo" }, JSON.stringify({ symbol: "foo", text, cut: false }));
    press("Enter");
    expect(body()?.textContent).toBe("**hover** <img src=x> [link](https://example.test)⟨U+202E⟩⟨U+200B⟩");
    expect(body()?.querySelector("img, a, strong")).toBeNull();
    press("o", true);
    expect(body()?.textContent).toContain('"text":"**hover**');
    expect(body()?.textContent).toContain("⟨U+202E⟩⟨U+200B⟩");
    expect(body()?.querySelector("img, a, strong")).toBeNull();
  });

  it.each([
    ["mcp__eitri__read_buffer", { path: "a.py" }, "bad JSON {", false],
    ["mcp__eitri__read_buffer", { path: "a.py" }, raw, true],
    ["mcp__eitri__nvim_exec", { kind: "lua", code: "return 2" }, raw, false],
    ["mcpToolCall", { server: "other", tool: "read_buffer", arguments: {} }, raw, false],
    ["acp:read", { title: "Agent's own title" }, raw, false],
  ])("keeps the existing body for %s (%j)", (name, input, content, isError) => {
    const { body, press } = mount(name, input, content, isError);
    press("Enter");
    expect(body()?.textContent).toBe(content);
  });

  it("expanding and toggling details sends no tool-result content back to the host", () => {
    // The page receives results for display; the host owns delivery to the model.
    const { press, posted } = mount();
    posted.length = 0;
    press("Enter"); press("o", true); press("o", true);
    expect(posted).toEqual([]);
  });
});
