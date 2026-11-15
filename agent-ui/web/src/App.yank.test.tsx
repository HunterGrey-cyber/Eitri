// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { panelDiagrams } from "./diagrams";
import App from "./App";
import { initialState } from "./reducer";
import { hintTargets } from "./nav";
import { WHICH_KEY_DELAY_MS } from "./leader";
import type { AgentUiState } from "./types";

let posted: Record<string, unknown>[];
let writeText: ReturnType<typeof vi.fn>;
let resize: { callback: ResizeObserverCallback; targets: Element[] }[];
beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  posted = [];
  resize = [];
  vi.stubGlobal("ResizeObserver", class {
    record: typeof resize[number];
    constructor(callback: ResizeObserverCallback) { this.record = { callback, targets: [] }; resize.push(this.record); }
    observe(el: Element) { this.record.targets.push(el); }
    unobserve() {}
    disconnect() {}
  });
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } } };
  writeText = vi.fn();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});
afterEach(() => {
  cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals();
  delete (window as unknown as { webkit?: unknown }).webkit;
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});
const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
const tab = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null };
function started(state: Partial<AgentUiState> = {}, dashboard = false) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, ...(dashboard ? { state: "not_started" } : {}) }] });
  if (!dashboard) dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" }, ...state } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  const root = view.container.querySelector<HTMLElement>(dashboard ? ".empty-tab" : ".agent-ui-conversation")!;
  act(() => {
    for (const { callback, targets } of resize) for (const target of targets) {
      if (target.matches(".status-band, .band-measure")) callback([{ target, contentRect: { width: target.matches(".band-measure") ? 7.2 : 1400 } } as ResizeObserverEntry], {} as ResizeObserver);
    }
  });
  const key = (key: string, extra = {}) => fireEvent.keyDown(root, { key, shiftKey: /^[A-Z{}]$/.test(key), ...extra });
  const keys = (text: string) => { for (const ch of text) key(ch); };
  return { ...view, root, key, keys };
}
const bash: AgentUiState["toolCalls"][number] = { seq: 1, toolUseId: "b", name: "Bash", input: { command: "echo hello" }, result: { content: "hello", isError: false } };

it("yim copies message source through clipboard and primary, while y alone copies nothing", () => {
  const { container, keys, key } = started({ transcript: [{ seq: 1, text: "**hello**" }] });
  key("y"); expect(writeText).not.toHaveBeenCalled(); keys("im");
  expect(writeText).toHaveBeenLastCalledWith("**hello**");
  expect(posted).toContainEqual({ type: "copy_primary", text: "**hello**" });
  expect(container.querySelector(".band-message")?.textContent).toBe("copied 9 chars");
});

it("yio copies a tool's full output and yac copies a permission's named input", () => {
  const { keys } = started({ toolCalls: [bash], pendingPermissions: [{ seq: 2, permissionId: "p", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }] });
  keys("ggyio"); expect(writeText).toHaveBeenLastCalledWith("hello");
  keys("jyac"); expect(writeText).toHaveBeenLastCalledWith("Bash: rm build");
});

it("y2j copies three rows without moving; Y copies the row", () => {
  const { container, keys, key } = started({ userPrompts: [{ seq: 1, text: "one" }, { seq: 2, text: "two" }, { seq: 3, text: "three" }, { seq: 4, text: "four" }] });
  keys("ggy2j");
  expect(writeText).toHaveBeenLastCalledWith("one\n\ntwo\n\nthree");
  expect(container.querySelector(".row-current")?.textContent).toContain("one");
  key("Y"); expect(writeText).toHaveBeenLastCalledWith("one");
});

it("yy on a HINT-landed code block copies its original code once", () => {
  const { root, container, keys, key } = started({ transcript: [{ seq: 1, text: "before\n\n```rust\nlet x = 1;\n```\n\nafter" }] });
  const block = container.querySelector<HTMLElement>("pre.code-block")!;
  for (const el of [root, ...root.querySelectorAll<HTMLElement>(".message-list, .row, pre")]) {
    el.getBoundingClientRect = () => ({ top: 0, bottom: 100, left: 0, right: 100, width: 100, height: 100, x: 0, y: 0 }) as DOMRect;
  }
  const index = hintTargets(root).findIndex((t) => t.el === block);
  expect(index).toBeGreaterThanOrEqual(0);
  dispatch({ kind: "hint_collect", sessionId: 1 }); dispatch({ kind: "hint_land", sessionId: 1, index });
  key("y"); expect(writeText).not.toHaveBeenCalled(); key("y");
  expect(writeText).toHaveBeenLastCalledWith("let x = 1;");
  keys("yy"); expect(writeText.mock.calls.at(-1)?.[0]).toContain("after");
  dispatch({ kind: "hint_collect", sessionId: 2 }); dispatch({ kind: "hint_land", sessionId: 2, index });
  keys("1yy"); expect(writeText.mock.calls.at(-1)?.[0]).toContain("after");
});

it("yiw opens no INPUT, types nothing, and reports the caret object", () => {
  const { container, keys } = started({ transcript: [{ seq: 1, text: "hello" }] });
  keys("yiw");
  expect(writeText).not.toHaveBeenCalled(); expect(container.querySelector("textarea")).toBeNull();
  expect(container.querySelector(".band-message")?.textContent).toBe("iw: needs the caret — v, then move, then y");
  expect(posted.some((m) => m.type === "send_message" || m.type === "draft")).toBe(false);
});

it("after yy, a motion within 250 ms is not swallowed", () => {
  const { container, keys, key } = started({ userPrompts: [{ seq: 1, text: "one" }, { seq: 2, text: "two" }] });
  keys("ggyy"); act(() => vi.advanceTimersByTime(60)); key("j");
  expect(container.querySelector(".row-current")?.textContent).toContain("two");
});

it.each(["Enter", " "])("y then %s on a focused Dismiss activates nothing, even after a pause", (key) => {
  const { container, root } = started({ transcript: [{ seq: 1, text: "hello" }] });
  dispatch({ kind: "command_result", requestId: "refused", ok: false, error: "cannot run" });
  const dismiss = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Dismiss")!;
  expect(dismiss).toBeDefined(); act(() => dismiss.focus());
  fireEvent.keyDown(dismiss, { key: "y" }); act(() => vi.advanceTimersByTime(500));
  expect(fireEvent.keyDown(dismiss, { key })).toBe(false);
  expect(container.querySelector(".command-notice")).not.toBeNull(); expect(writeText).not.toHaveBeenCalled();
  fireEvent.keyDown(root, { key: "y" }); expect(writeText).not.toHaveBeenCalled();
});

it("shows the y box only after the which-key delay and drops it on a landing", () => {
  const { container, key } = started({ transcript: [{ seq: 1, text: "hello" }] });
  key("y"); expect(container.querySelector(".which-key-box")).toBeNull();
  act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
  expect(container.textContent).toContain("message source");
  dispatch({ kind: "arrive" });
  key("y"); expect(writeText).not.toHaveBeenCalled();
});

it("two code blocks open a plain-letter pick; repeats wait and modified keys cancel", () => {
  const { container, keys, key } = started({ transcript: [{ seq: 1, text: "```js\nfirst\n```\n\n```rust\nsecond\n```" }] });
  keys("yi`"); expect(container.querySelector(".path-pick")?.textContent).toContain("code block 2 (rust)");
  key("s", { repeat: true }); expect(writeText).not.toHaveBeenCalled();
  key("s"); expect(writeText).toHaveBeenLastCalledWith("second");
  keys("ya`"); key("a"); expect(writeText).toHaveBeenLastCalledWith("```js\nfirst\n```");
  keys("ya`"); key("a", { ctrlKey: true }); expect(container.querySelector(".path-pick")).toBeNull();
  expect(writeText).toHaveBeenCalledTimes(2);
});

it.each(["yi`", "gf"])("%s yields to typing in a card's reason input", (command) => {
  const { container, keys } = started({
    transcript: [{ seq: 1, text: "src/a.rs src/b.rs\n\n```js\nfirst\n```\n\n```rust\nsecond\n```" }],
    pendingPermissions: [{ seq: 2, permissionId: "p", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
  });
  keys("gg" + command);
  expect(container.querySelector(".path-pick")).not.toBeNull();
  const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
  act(() => reason.focus());
  const allowed = fireEvent.keyDown(reason, { key: "a" });
  // jsdom has no native text editing; perform it only if keydown left the default action available.
  if (allowed) fireEvent.change(reason, { target: { value: reason.value + "a" } });
  expect(allowed).toBe(true);
  expect(reason.value).toBe("a");
  expect(container.querySelector(".path-pick")).toBeNull();
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.some((m) => m.type === "copy_primary" || m.type === "open_path")).toBe(false);
});

it.each(["acp:edit", "fileChange"])("%s hunk picks anchor on their own files after a count-only file", (name) => {
  const input = name === "acp:edit" ? { diffs: [
    { path: "large", oldText: "x\n".repeat(801), newText: "y" },
    { path: "A", oldText: "a", newText: "A" },
    { path: "B", oldText: "b", newText: "B" },
  ] } : { changes: [
    { path: "large", kind: "add", diff: "x\n".repeat(801) },
    { path: "A", kind: "update", diff: "@@ -1 +1 @@\n-a\n+A\n" },
    { path: "B", kind: "update", diff: "@@ -1 +1 @@\n-b\n+B\n" },
  ] };
  const { root, container, keys, key } = started({ toolCalls: [{ ...bash, name, input, result: { content: "applied", isError: false } }] });
  key("Enter");
  const rect = (top: number, height: number) => ({ top, bottom: top + height, left: 0, right: 500, width: 500, height, x: 0, y: top }) as DOMRect;
  for (const el of [root, ...root.querySelectorAll<HTMLElement>(".message-list, .row")]) {
    el.getBoundingClientRect = () => rect(0, 1000);
    Object.defineProperties(el, { clientWidth: { configurable: true, value: 500 }, clientHeight: { configurable: true, value: 1000 } });
  }
  const files = [...container.querySelectorAll<HTMLElement>(".permission-card-edit")];
  expect(files).toHaveLength(3);
  files.forEach((file, i) => file.querySelectorAll<HTMLElement>(".diff-line").forEach((line) => {
    line.getBoundingClientRect = () => rect(100 + i * 100, 20);
  }));
  keys("yih");
  const labels = [...container.querySelectorAll<HTMLElement>(".hint-code")];
  expect(labels.map((label) => [label.textContent, label.style.top])).toEqual([["a", "204px"], ["s", "304px"]]);
  key("s");
  expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  keys("]cyih");
  expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
  keys("]cyih");
  expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
});

it("the dashboard keeps its y copy burst, including Shift+Tab", () => {
  const { container, root, key } = started({}, true);
  dispatch({ kind: "handoff", tab: 1, command: "echo handoff", cwd: "/p", providerSessionId: "s" });
  key("Escape"); act(() => vi.advanceTimersByTime(1000));
  key("y"); expect(writeText).toHaveBeenLastCalledWith("echo handoff"); key("i");
  expect(container.querySelector("textarea")).toBeNull();
  expect(container.querySelector(".band-message")?.textContent).toBe("i or Ctrl+j to type");
  fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
  expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(0);
});

it("Shift+Tab and Alt+n discard an operator", () => {
  const { keys, key } = started({ userPrompts: [{ seq: 1, text: "hello" }] });
  key("y"); key("Tab", { shiftKey: true }); key("y");
  expect(writeText).not.toHaveBeenCalled();
  key("n", { altKey: true }); key("y"); expect(writeText).not.toHaveBeenCalled();
  keys("y"); expect(writeText).toHaveBeenLastCalledWith("hello");
});

it("a y object never reaches a configured leader and clicking its which-key row completes it", () => {
  const { container, key } = started({ transcript: [{ seq: 1, text: "source" }] });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], panel: { leader: "i", leaderLabel: "i", leaderSource: "config", timeoutlen: 300, timeout: true, bindings: [{ keys: ["<leader>", "m"], action: "tab.new", desc: "new tab", source: "config" }], groups: [] } });
  key("y"); act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
  const im = Array.from(container.querySelectorAll<HTMLElement>(".wk-entry")).find((el) => el.querySelector(".wk-key")?.textContent === "im")!;
  fireEvent.click(im);
  expect(writeText).toHaveBeenLastCalledWith("source");
  expect(posted.some((m) => m.type === "new_tab")).toBe(false);
});

it("long labels support Backspace, plain-letter selection, and no selection on Enter", () => {
  const text = Array.from({ length: 16 }, (_, i) => `a${i}.rs`).join(" ");
  const { keys, key, container } = started({ transcript: [{ seq: 1, text }] });
  keys("yif"); key("a"); key("Backspace"); expect(writeText).not.toHaveBeenCalled();
  expect(container.querySelector(".path-pick")?.textContent).toContain("a15.rs");
  keys("as"); expect(writeText).toHaveBeenLastCalledWith("a1.rs");
  keys("yif"); key("Enter"); expect(writeText).toHaveBeenCalledTimes(1);
  expect(container.querySelector(".path-pick")).toBeNull();
});

it.each(["yif", "yaf"])("%s refuses stripped and relative links", (command) => {
  const { keys, container } = started({ transcript: [{ seq: 1, text: "[bad](javascript:alert(1)) [relative](/relative)" }] });
  keys(command);
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.some((m) => m.type === "copy_primary")).toBe(false);
  expect(container.querySelector(".band-message")?.textContent).toContain("not a path or URL");
});

it("shows a hidden address before yif copies its normalized URL", () => {
  const { keys, key, container } = started({ transcript: [{ seq: 1, text: "[friendly](https://EXAMPLE.org)" }] });
  keys("yif");
  expect(writeText).not.toHaveBeenCalled();
  expect(container.querySelector(".path-pick")?.textContent).toContain("https://example.org/");
  key("a");
  expect(writeText).toHaveBeenLastCalledWith("https://example.org/");
  expect(posted).toContainEqual({ type: "copy_primary", text: "https://example.org/" });
});

it("positions URL pick labels on the rendered anchors after normalization", () => {
  const { root, keys, container } = started({ transcript: [{ seq: 1, text: "[one](https://EXAMPLE.org) [two](https://OTHER.org)" }] });
  for (const el of [root, ...root.querySelectorAll<HTMLElement>(".message-list, .row, a")]) {
    el.getBoundingClientRect = () => ({ top: 0, bottom: 200, left: 0, right: 200, width: 200, height: 200, x: 0, y: 0 }) as DOMRect;
    Object.defineProperties(el, { clientWidth: { configurable: true, value: 200 }, clientHeight: { configurable: true, value: 200 } });
  }
  keys("yif");
  expect(container.querySelectorAll(".hint-link")).toHaveLength(2);
  expect(container.querySelector(".path-pick")?.textContent).toContain("https://example.org/");
});

it.each(["yim", "yam", "yy", "Y"])("%s preserves both selections on an empty message", (command) => {
  const { keys, container } = started({ transcript: [{ seq: 1, text: "" }] });
  keys(command);
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.some((m) => m.type === "copy_primary")).toBe(false);
  expect(container.querySelector(".band-message")?.textContent).toMatch(/empty|nothing to copy/);
});

it.each(["yio", "yao"])("%s preserves both selections on empty tool output", (command) => {
  const { keys, container } = started({ toolCalls: [{ ...bash, result: { content: "", isError: false } }] });
  keys(command);
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.some((m) => m.type === "copy_primary")).toBe(false);
  expect(container.querySelector(".band-message")?.textContent).toContain("this tool call has no output");
});

it.each([["y[[", "earlier"], ["y]]", "later"]])("%s refuses a missing prompt destination", (command, direction) => {
  const { keys, container } = started({ userPrompts: [{ seq: 1, text: "only prompt" }] });
  keys(command);
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.some((m) => m.type === "copy_primary")).toBe(false);
  expect(container.querySelector(".band-message")?.textContent).toBe(`no ${direction} prompt`);
});

it.each(["y2G", "2yG", "y2gg", "2ygg"])("%s copies through row N without moving", (command) => {
  const { keys, container } = started({ userPrompts: [{ seq: 1, text: "one" }, { seq: 2, text: "two" }, { seq: 3, text: "three" }, { seq: 4, text: "four" }] });
  keys("gg" + command);
  expect(writeText).toHaveBeenLastCalledWith("one\n\ntwo");
  expect(container.querySelector(".row-current")?.textContent).toContain("one");
});

it("ya` keeps the rendered block's language when an earlier block is absent", () => {
  const { keys, container } = started({ transcript: [{ seq: 1, text: "<select>\n\n```js\nfirst\n```\n\n</select>\n\n```rust\nsecond\n```" }] });
  expect(container.querySelectorAll("pre.code-block")).toHaveLength(1);
  keys("ya`");
  expect(writeText).toHaveBeenLastCalledWith("```rust\nsecond\n```");
});

it.each(["yi`", "gy", "ya`"])("%s takes the current HINT-landed block among several", (command) => {
  const { root, keys, container } = started({ transcript: [{ seq: 1, text: "```js\nfirst\n```\n\n```rust\nsecond\n```" }] });
  const block = container.querySelectorAll<HTMLElement>("pre.code-block")[1];
  for (const el of [root, ...root.querySelectorAll<HTMLElement>(".message-list, .row, pre")]) {
    el.getBoundingClientRect = () => ({ top: 0, bottom: 100, left: 0, right: 100, width: 100, height: 100, x: 0, y: 0 }) as DOMRect;
  }
  const index = hintTargets(root).findIndex((t) => t.el === block);
  expect(index).toBeGreaterThanOrEqual(0);
  dispatch({ kind: "hint_collect", sessionId: 1 }); dispatch({ kind: "hint_land", sessionId: 1, index });
  keys(command);
  expect(container.querySelector(".path-pick")).toBeNull();
  expect(writeText).toHaveBeenLastCalledWith(command === "ya`" ? "```rust\nsecond\n```" : "second");
});

it("drawing code objects and HINT copy source, while row yank puts fenced source in place", () => {
  const source = "flowchart LR; Source-->Destination";
  const { root, container, keys, key } = started({ transcript: [{ seq: 1, text: `before\n\n\`\`\`mermaid\n${source}\n\`\`\`\n\nafter` }] });
  dispatch({ kind: "theme", vars: {}, diagramGeneration: 777 });
  dispatch({ kind: "display", thinking: "collapsed", diagrams: true });
  const request = posted.find((m) => m.type === "render_diagram")!;
  expect(request.source).toBe(source);
  dispatch({ kind: "diagram", id: request.id, svg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100"><text>drawing</text></svg>' });
  expect(container.querySelector("pre.diagram-drawn img")).not.toBeNull();
  keys("gy"); expect(writeText).toHaveBeenLastCalledWith(source);
  keys("yi`"); expect(writeText).toHaveBeenLastCalledWith(source);
  keys("yy");
  const copied = writeText.mock.calls.at(-1)![0];
  expect(copied).toContain("before"); expect(copied).toContain("```mermaid\n" + source + "\n```"); expect(copied).toContain("after");
  for (const el of [root, ...root.querySelectorAll<HTMLElement>(".message-list, .row, pre")])
    el.getBoundingClientRect = () => ({ top: 0, bottom: 100, left: 0, right: 100, width: 100, height: 100, x: 0, y: 0 }) as DOMRect;
  const block = container.querySelector<HTMLElement>("pre.code-block")!;
  const index = hintTargets(root).findIndex((target) => target.el === block);
  dispatch({ kind: "hint_collect", sessionId: 901 }); dispatch({ kind: "hint_land", sessionId: 901, index });
  key("y"); key("y"); expect(writeText).toHaveBeenLastCalledWith(source);
});

it.each(["v", "vv", "V"])("%s holds an arriving drawing until the selection mode ends", (selectionKey) => {
  const selection = window.getSelection()!;
  Object.defineProperty(selection, "modify", { value: vi.fn(), configurable: true });
  try {
    const source = "flowchart LR; Read-->Hold" + selectionKey;
    const { container, key } = started({ transcript: [{ seq: 1, text: `before\n\n\`\`\`mermaid\n${source}\n\`\`\`\n\nafter` }] });
    dispatch({ kind: "display", thinking: "collapsed", diagrams: true });
    const request = posted.find((m) => m.type === "render_diagram")!;
    for (const char of selectionKey) key(char);
    dispatch({ kind: "diagram", id: request.id, svg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100"><text>held</text></svg>' });
    expect(container.querySelector("pre img")).toBeNull();
    key("Escape");
    if (selectionKey !== "v") expect(container.querySelector("pre img")).toBeNull();
    key("Escape");
    expect(container.querySelector("pre img")).not.toBeNull();
  } finally {
    delete (selection as unknown as { modify?: unknown }).modify;
  }
});

it.each([true, false])("an unchanged App render with 300 replies does no diagram attach work (enabled=%s), including a plan transcript fence", (enabled) => {
  const source = "flowchart LR; Plan-->Code";
  const { container, key } = started({
    transcript: Array.from({ length: 300 }, (_, seq) => ({ seq: seq + 2, text: "reply " + seq })),
    plans: [{ seq: 1, interactionId: "plan1", toolUseId: null, turnId: null, plan: "```mermaid\n" + source + "\n```\n", planSha256: "hash", allowedPrompts: [], state: { kind: "settled", outcome: "approved", modeAfter: null, feedback: null, reason: null }, unsupportedReason: null }],
  });
  dispatch({ kind: "display", thinking: "collapsed", diagrams: enabled });
  key("g"); key("g"); key("z"); key("o");
  expect(container.querySelector(".plan-row-body pre")?.textContent).toContain(source);
  expect(container.querySelector(".plan-row-body [data-code-diagram]")).toBeNull();
  const attach = vi.spyOn(panelDiagrams, "attach");
  const freeze = vi.spyOn(panelDiagrams, "freeze");
  dispatch({ kind: "pane_focus", focused: true });
  expect(freeze).not.toHaveBeenCalled();
  expect(attach).not.toHaveBeenCalled();
});

it("hidden-character diagram notes never enter gy, code y or row yy", () => {
  const source = "flowchart LR; A-->B\u202e";
  const { container, keys } = started({ transcript: [{ seq: 1, text: "```mermaid\n" + source + "\n```\n" }] });
  dispatch({ kind: "display", thinking: "collapsed", diagrams: true });
  expect(container.querySelector(".diagram-error")).not.toBeNull();
  keys("gy"); expect(writeText).toHaveBeenLastCalledWith(source);
  keys("yi`"); expect(writeText).toHaveBeenLastCalledWith(source);
  keys("yy"); expect(writeText.mock.calls.at(-1)![0]).not.toContain("Diagram not drawn:");
});
