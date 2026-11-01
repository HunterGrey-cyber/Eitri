// @vitest-environment jsdom
/**
 * The note a window shows when its project folder is no boundary (the `root_note` envelope): a banner of its
 * own, cleared only by its Dismiss, never by a send, a snapshot or a command's notice.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { Hello } from "./types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "not_started", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;
const NOTE =
  "No project boundary here (it is open to others): a Claude tab asks before every file and command, and a Codex " +
  "tab can write anywhere in this folder.";

const banners = (c: HTMLElement) => Array.from(c.querySelectorAll<HTMLElement>(".root-note"));
const notices = (c: HTMLElement) => Array.from(c.querySelectorAll<HTMLElement>(".command-notice:not(.root-note)"));
const dismissOf = (banner: HTMLElement) => banner.querySelector("button")!;

function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "root_note", text: NOTE });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  return rendered;
}

describe("the root note", () => {
  it("shows the banner with the text it was given", () => {
    const { container } = started();
    expect(banners(container)).toHaveLength(1);
    expect(banners(container)[0].textContent).toContain(NOTE);
  });

  it("shows nothing when no root_note arrived", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
    expect(banners(container)).toHaveLength(0);
  });

  it("stays through a send and the snapshot that follows it", () => {
    const { container } = started();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "fix the bug" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(posted.some((m) => m.type === "send_message")).toBe(true);
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "live" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } });
    expect(banners(container)).toHaveLength(1);
  });

  it("stays when a command notice comes and when that notice is dismissed", () => {
    const { container } = started();
    dispatch({ kind: "command_result", requestId: "r1", ok: false, error: "A turn is still running." });
    expect(notices(container)).toHaveLength(1);
    expect(banners(container)).toHaveLength(1);
    fireEvent.click(dismissOf(notices(container)[0]));
    expect(notices(container)).toHaveLength(0);
    expect(banners(container)).toHaveLength(1);
  });

  it("goes with its own Dismiss, and a command notice does not bring it back", () => {
    const { container } = started();
    fireEvent.click(dismissOf(banners(container)[0]));
    expect(banners(container)).toHaveLength(0);
    dispatch({ kind: "command_result", requestId: "r2", ok: false, error: "no active session" });
    expect(banners(container)).toHaveLength(0);
    expect(notices(container)).toHaveLength(1);
  });

  it("is drawn again when the note is told again", () => {
    const { container } = started();
    fireEvent.click(dismissOf(banners(container)[0]));
    dispatch({ kind: "root_note", text: NOTE });
    expect(banners(container)).toHaveLength(1);
  });
});
