// @vitest-environment jsdom
/**
 * The note a window shows when its project folder is no boundary (the `root_note` envelope): a banner of its
 * own, cleared only by its Dismiss, never by a send, a snapshot or a command's notice. A remote tab's own note
 * names its tab: it is drawn only while that tab is shown, and its Dismiss holds for that tab until its text
 * changes.
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

const TAB2 = { ...TAB, id: 2, number: 2, label: "2 new" } as const;
const REMOTE = "this tab runs on example-host; path checks on the server are not available yet";

/** Tab 1 is local, tab 2 remote; `windowNote` says whether the window's own folder is no boundary. */
function mixed(windowNote: boolean) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  if (windowNote) dispatch({ kind: "root_note", text: NOTE });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB, TAB2] });
  return rendered;
}

/** A switch as the host sends it: the strip first, then the remote tab's own note. */
function show(tab: 1 | 2, note: string | null = tab === 2 ? REMOTE : null) {
  dispatch({ kind: "tabs", active: tab, tabs: [TAB, TAB2] });
  if (note !== null) dispatch({ kind: "root_note", tab, text: note });
}

const texts = (c: HTMLElement) => banners(c).map((b) => b.textContent);

describe("a remote tab's note", () => {
  it("is drawn only while its tab is shown", () => {
    const { container } = mixed(false);
    expect(banners(container)).toHaveLength(0);
    show(2);
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
    show(1);
    expect(banners(container)).toHaveLength(0);
    show(2);
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
  });

  it("gives way to the window's own note on a local tab", () => {
    const { container } = mixed(true);
    expect(texts(container)).toEqual([`${NOTE}Dismiss`]);
    show(2);
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
    show(1);
    expect(texts(container)).toEqual([`${NOTE}Dismiss`]);
  });

  it("stays dismissed for its tab when told again, and comes back only with other text", () => {
    const { container } = mixed(true);
    show(2);
    fireEvent.click(dismissOf(banners(container)[0]));
    expect(banners(container)).toHaveLength(0);
    // Its Dismiss is the remote tab's own: the window's note is still there for the local tab.
    show(1);
    expect(texts(container)).toEqual([`${NOTE}Dismiss`]);
    show(2);
    expect(banners(container)).toHaveLength(0);
    dispatch({ kind: "root_note", tab: 2, text: REMOTE });
    expect(banners(container)).toHaveLength(0);
    show(2, "this tab runs on other-host; path checks on the server are not available yet");
    expect(texts(container)).toEqual(["this tab runs on other-host; path checks on the server are not available yet" + "Dismiss"]);
  });

  it("tells the window of its Dismiss, and a note the window says was dismissed is drawn dismissed", () => {
    const { container } = mixed(false);
    show(2);
    fireEvent.click(dismissOf(banners(container)[0]));
    expect(posted.filter((p) => p.type === "root_note_dismissed")).toEqual([
      expect.objectContaining({ tab: 2, text: REMOTE }),
    ]);
    cleanup();
    // Another page the tab moves into is told the note already dismissed.
    const other = mixed(false);
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, TAB2] });
    dispatch({ kind: "root_note", tab: 2, text: REMOTE, dismissed: true });
    expect(banners(other.container)).toHaveLength(0);
  });

  it("is taken down when told empty, giving way to the window's note, and the window's note is not", () => {
    const { container } = mixed(true);
    show(2);
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
    dispatch({ kind: "root_note", tab: 2, text: "" });
    expect(texts(container)).toEqual([`${NOTE}Dismiss`]);
    // An empty window note changes nothing.
    dispatch({ kind: "root_note", text: "" });
    expect(texts(container)).toEqual([`${NOTE}Dismiss`]);
    // Told again later, it is drawn again.
    dispatch({ kind: "root_note", tab: 2, text: REMOTE });
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
  });

  it("dismissing the window's note leaves the remote tab's", () => {
    const { container } = mixed(true);
    fireEvent.click(dismissOf(banners(container)[0]));
    expect(banners(container)).toHaveLength(0);
    show(2);
    expect(texts(container)).toEqual([`${REMOTE}Dismiss`]);
    show(1);
    expect(banners(container)).toHaveLength(0);
  });
});
