// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { initialState } from "./reducer";
import { revealedText } from "./revealHidden";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });
let posted: Array<Record<string, unknown>>;
let measureBand: () => void;
beforeEach(() => {
  posted = [];
  const measurements: Array<() => void> = [];
  vi.stubGlobal("ResizeObserver", class {
    constructor(private callback: ResizeObserverCallback) {}
    observe(target: Element) {
      measurements.push(() => this.callback([{ target, contentRect: {
        width: target.classList.contains("band-measure") ? 7.2 : 2000,
      } } as unknown as ResizeObserverEntry], this as unknown as ResizeObserver));
    }
    unobserve() {}
    disconnect() {}
  });
  measureBand = () => act(() => { measurements.forEach(measure => measure()); });
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});
function dispatch(payload: unknown) {
  act(() => { window.__eitriDispatch!(JSON.stringify(payload)); });
  measureBand();
}
const tab = {
  id: 1, number: 1, label: "1 api", name: null, state: "ended", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
};
function started() {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/srv/project", permissionModes: ["auto"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [tab] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: {
    ...initialState(), status: { kind: "unavailable", reason: "the server's journal no longer holds the events that were missed" },
  } });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  return view;
}
const gap = {
  kind: "remote_prompt", tab: 1, nonce: 7,
  lines: ["the agent on example-host may still be running; these requests wait there for an answer:", "Bash: rm -r build"],
  choices: [
    { key: "s", label: "deny them and stop the agent", action: "deny_and_stop" },
    { key: "L", label: "leave it waiting on example-host", action: "leave_waiting" },
  ],
};
const answers = () => posted.filter((p) => p.type === "remote_prompt_answer");
const root = (container: HTMLElement) => container.querySelector(".agent-ui-conversation")!;
const band = (container: HTMLElement) => container.querySelector(".band-prompt")?.textContent ?? null;
/** jsdom stamps keys with `Date.now()` and the guard reads `performance.now()`: both move with the timers. */
function fakeClock() {
  vi.useFakeTimers({
    toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "clearImmediate", "Date", "performance"],
  });
}
const pause = () => act(() => { vi.advanceTimersByTime(300); });
afterEach(() => { vi.useRealTimers(); });

it("lists the waiting requests and answers with the choice its key names, echoing the tab and nonce", () => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  expect(band(container)).toContain("Bash: rm -r build");
  expect(band(container)).toContain("s deny them and stop the agent · L leave it waiting on example-host");
  pause();
  fireEvent.keyDown(root(container), { key: "s" });
  expect(answers()).toEqual([expect.objectContaining({ tab: 1, nonce: 7, action: "deny_and_stop" })]);
  expect(container.querySelector(".band-prompt")).toBeNull();
});

it("draws the agent's words in the question with their hidden and bidi characters shown, not obeyed", () => {
  fakeClock();
  const { container } = started();
  const tool = "Ba\u200bsh";
  const input = "echo safe \u202e; rm -r build";
  dispatch({ ...gap, lines: [gap.lines[0], `${tool}: ${input}`] });
  const shown = band(container)!;
  expect(shown).not.toContain("\u202e");
  expect(shown).not.toContain("\u200b");
  expect(shown).toContain(revealedText(`${tool}: ${input}`));
});

it.each(["x", "y", "j", "d", "l"])("%s chooses nothing and leaves the question up", (key) => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  pause();
  fireEvent.keyDown(root(container), { key });
  expect(answers()).toEqual([]);
  expect(band(container)).toContain("Bash: rm -r build");
});

it("Escape puts the question away until its tab is shown again", () => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  fireEvent.keyDown(root(container), { key: "Escape" });
  expect(answers()).toEqual([]);
  expect(container.querySelector(".band-prompt")).toBeNull();
  dispatch({ kind: "tabs", active: 2, tabs: [tab, { ...tab, id: 2, number: 2, label: "2 web" }] });
  dispatch({ kind: "tabs", active: 1, tabs: [tab, { ...tab, id: 2, number: 2, label: "2 web" }] });
  expect(band(container)).toContain("Bash: rm -r build");
});

it("a key typed into a text field is the field's: it neither answers nor puts the question away", () => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  pause();
  const field = document.createElement("textarea");
  root(container).appendChild(field);
  field.focus();
  for (const key of ["a", "s", "s", "L"]) fireEvent.keyDown(field, { key });
  expect(answers()).toEqual([]);
  expect(band(container)).toContain("Bash: rm -r build");
});

it("a held key, one with a modifier, or one too soon after the question or another key chooses nothing", () => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  fireEvent.keyDown(root(container), { key: "s" });
  expect(answers()).toEqual([]);
  pause();
  fireEvent.keyDown(root(container), { key: "s", ctrlKey: true });
  pause();
  fireEvent.keyDown(root(container), { key: "s", repeat: true });
  pause();
  fireEvent.keyDown(root(container), { key: "h" });
  fireEvent.keyDown(root(container), { key: "s" });
  expect(answers()).toEqual([]);
  expect(band(container)).toContain("Bash: rm -r build");
});

it("a background tab's question waits until its tab is shown, and names its tab", () => {
  fakeClock();
  const { container } = started();
  const other = { ...tab, id: 2, number: 2, label: "2 web", state: "live" };
  dispatch({ kind: "tabs", active: 2, tabs: [tab, other] });
  dispatch({ ...gap, lines: ["tab 1: the agent on example-host may still be running", "Bash: rm -r build"] });
  expect(container.querySelector(".band-prompt")).toBeNull();
  fireEvent.keyDown(root(container), { key: "s" });
  pause();
  fireEvent.keyDown(root(container), { key: "s" });
  expect(answers()).toEqual([]);
  dispatch({ kind: "tabs", active: 1, tabs: [tab, other] });
  expect(band(container)).toContain("tab 1:");
});

it("its arrival does not replace a question already up; it is drawn once that one is answered", () => {
  fakeClock();
  const { container } = started();
  dispatch({ kind: "confirm_restore", nonce: 3, lines: ["restore 1 tab in bypass?"] });
  dispatch(gap);
  expect(band(container)).toContain("restore 1 tab in bypass?");
  fireEvent.keyDown(root(container), { key: "n" });
  expect(posted.filter((p) => p.type === "restore_answer")).toHaveLength(1);
  expect(band(container)).toContain("Bash: rm -r build");
});

it("the close and resume questions answer with their own choices", () => {
  fakeClock();
  const { container } = started();
  dispatch({
    kind: "remote_prompt", tab: 1, nonce: 9, lines: ["tab 1: example-host cannot be reached; its agent keeps running there"],
    choices: [
      { key: "s", label: "stop it when example-host is back", action: "stop_when_back" },
      { key: "L", label: "leave it running", action: "leave_running" },
    ],
  });
  pause();
  fireEvent.keyDown(root(container), { key: "s" });
  dispatch({
    kind: "remote_prompt", tab: 1, nonce: 10, lines: ["tab 1: example-host no longer runs this tab's agent"],
    choices: [{ key: "R", label: "resume the session on example-host", action: "resume" }],
  });
  pause();
  fireEvent.keyDown(root(container), { key: "R", shiftKey: true });
  expect(answers().map((a) => [a.nonce, a.action])).toEqual([[9, "stop_when_back"], [10, "resume"]]);
});

it("a question Rust no longer holds is dropped, from the screen too, and a closed tab's goes with it", () => {
  fakeClock();
  const { container } = started();
  dispatch(gap);
  dispatch({ kind: "remote_prompt_clear", tab: 1, nonce: 6 });
  expect(band(container)).toContain("Bash: rm -r build");
  dispatch({ kind: "remote_prompt_clear", tab: 1, nonce: 7 });
  expect(container.querySelector(".band-prompt")).toBeNull();
  const other = { ...tab, id: 2, number: 2, label: "2 web" };
  dispatch({ kind: "tabs", active: 2, tabs: [tab, other] });
  dispatch({ ...gap, nonce: 11 });
  dispatch({ kind: "tabs", active: 2, tabs: [other] });
  dispatch({ kind: "tabs", active: 2, tabs: [tab, other] });
  dispatch({ kind: "tabs", active: 1, tabs: [tab, other] });
  expect(container.querySelector(".band-prompt")).toBeNull();
});

it("a question with a choice Rust does not take, or no nonce, is dropped", () => {
  const { container } = started();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  dispatch({ ...gap, choices: [{ key: "a", label: "allow them", action: "allow" }] });
  dispatch({ ...gap, nonce: undefined });
  expect(container.querySelector(".band-prompt")).toBeNull();
  expect(warn).toHaveBeenCalledTimes(2);
});


it("reveals hidden characters in the too-soon choice label too", () => {
  fakeClock();
  const { container } = started();
  const label = "leave it waiting on example\u202e-host\u200b";
  dispatch({ ...gap, choices: [{ key: "L", label, action: "leave_waiting" }] });
  fireEvent.keyDown(root(container), { key: "L", shiftKey: true });
  expect(answers()).toEqual([]);
  expect(container.textContent).toContain(`to ${revealedText(label)}`);
  expect(container.textContent).not.toContain("\u202e");
  expect(container.textContent).not.toContain("\u200b");
});
