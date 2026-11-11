// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import type { AgentUiState, Hello } from "./types";

let posted: Array<Record<string, unknown>>;
let widen: (container: HTMLElement) => void;
let restoreModify: () => void;
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });
beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  widen = stubBandWidth();
  restoreModify = stubSelectionModify();
});
afterEach(() => {
  cleanup(); restoreModify(); window.getSelection()?.removeAllRanges();
  vi.useRealTimers(); vi.unstubAllGlobals();
});
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
const HELLO: Hello = { backend: "legacy", projectDir: "/tmp/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null };
const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null } as const;
function setup(text: string, reply = false) {
  const { container } = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", tabs: [TAB], active: 1 });
  const state: AgentUiState = { ...initialState(), status: { kind: "running" },
    ...(reply ? { transcript: [{ seq: 1, text }], assistantMessageOpen: true, nextSeq: 2 } : { userPrompts: [{ seq: 1, text }] }) };
  dispatch({ kind: "snapshot", tab: 1, revision: 2, state, queue: [], undo: [], panelKeys: EMPTY_PANEL_TABLE });
  dispatch({ kind: "pane_focus", focused: true });
  act(() => widen(container));
  const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string) => { fireEvent.keyDown(root, { key }); fireEvent.keyUp(root, { key }); };
  const gd = () => { press("g"); press("d"); };
  return { container, press, gd };
}
const last = (type: string) => posted.filter((p) => p.type === type).at(-1);
function answer(matches: unknown[], error?: unknown) {
  const request = last("editor_symbol")!;
  dispatch({ kind: "editor_symbol_result", requestId: request.request_id, matches, ...(error ? { error } : {}) });
}
const match = (path: string, line: number, source = "lsp") => ({ path, line, col: 1, source });
function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return (container: HTMLElement) => {
    const band = container.querySelector(".status-band");
    const measure = container.querySelector(".band-measure");
    for (const o of observers) {
      for (const el of o.observed) {
        if (el === band) o.callback([{ target: el, contentRect: { width: 1400 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}
function stubSelectionModify(): () => void {
  const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
  const original = proto.modify;
  proto.modify = function (this: Selection, alter: string, direction: string, granularity: string) {
    const node = this.focusNode;
    if (node === null) return;
    const text = node.textContent ?? "";
    let offset = this.focusOffset;
    const startAnchorNode = this.anchorNode;
    const startAnchorOffset = this.anchorOffset;
    if (granularity === "character") {
      offset = direction === "forward" ? Math.min(text.length, offset + 1) : Math.max(0, offset - 1);
      // The line-end caret quirk: at a line end WebKit draws the line-break box, which
      // reads like vim's cursor past the end. A real engine still shows a one-character block caret
      // at the very last position, which needs real layout jsdom does not have. A block caret
      // (anchor === focus, both already at `text.length`) extending forward has nowhere left to move
      // within this text node -- pull the ANCHOR back by one instead, so the stub still produces the
      // one-character selection the caret needs, without claiming jsdom modelled WebKit's own rendering.
      if (
        alter === "extend" &&
        direction === "forward" &&
        offset === this.focusOffset &&
        startAnchorNode === node &&
        startAnchorOffset === this.focusOffset &&
        offset > 0
      ) {
        this.setBaseAndExtent(node, offset - 1, node, offset);
        return;
      }
    } else if (granularity === "word") {
      if (direction === "forward") {
        let i = offset;
        while (i < text.length && /\S/.test(text[i]!)) i++;
        while (i < text.length && /\s/.test(text[i]!)) i++;
        offset = i;
      } else {
        let i = offset;
        while (i > 0 && /\s/.test(text[i - 1]!)) i--;
        while (i > 0 && /\S/.test(text[i - 1]!)) i--;
        offset = i;
      }
    } else {
      // "line" / "paragraphboundary": this stub's rows are each one text node, so both collapse to
      // that node's own start/end -- the same unit `0`/`$` and V-LINE share.
      offset = direction === "forward" ? text.length : 0;
    }
    if (alter === "move") this.collapse(node, offset);
    else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
  };
  return () => {
    if (original === undefined) delete proto.modify;
    else proto.modify = original;
  };
}


describe("gd on the page", () => {
  it("opens one BROWSE path with its expected code name", () => {
    const { gd } = setup("`serve_replay` at `mesh.go:142`", true); gd();
    expect(last("open_path")).toMatchObject({ path: "mesh.go", line: 142, expect: "serve_replay" });
    expect(last("editor_symbol")).toBeUndefined();
  });
  it("offers two BROWSE paths and preserves the expectation through a suffix choice", () => {
    const { gd, press, container } = setup("`serve_replay` at `a.go:10` and b.go:20", true); gd();
    expect(container.querySelector(".path-pick")?.textContent).toContain("a.go:10");
    press("a");
    expect(last("open_path")).toMatchObject({ path: "a.go", line: 10, expect: "serve_replay" });
    dispatch({ kind: "path_choices", requestId: last("open_path")!.request_id, tab: 1,
      paths: [{ path: "src/a.go", line: 10 }, { path: "test/a.go", line: 10 }] });
    press("s");
    expect(last("open_path")).toMatchObject({ path: "test/a.go", line: 10, expect: "serve_replay" });
    expect(last("open_path")!.interaction).toBe(last("panel_keys")!.interaction);
  });
  it("explains how to look up a name when the BROWSE row has no paths", () => {
    const { gd, container } = setup("Name"); gd();
    expect(container.textContent).toContain("gd: no path on this row — v, move to a name, gd");
    expect(last("editor_symbol")).toBeUndefined();
  });
  it("posts the caret's name and same-line citation", () => {
    const { gd, press } = setup("TestMeshFieldsOmittedWhenZero at mesh.go:142"); press("v"); gd();
    expect(last("editor_symbol")).toMatchObject({ tab: 1, name: "TestMeshFieldsOmittedWhenZero", cited: { path: "mesh.go", line: 142 } });
  });
  it("reads a cited name across rendered inline spans", () => {
    const { gd, press } = setup("`Name` at `mesh.go:142`", true); press("v"); gd();
    expect(last("editor_symbol")).toMatchObject({ name: "Name", cited: { path: "mesh.go", line: 142 } });
  });
  it("opens a caret pathline with an inline expected name", () => {
    const { gd, press } = setup("`mesh.go:142` has `Name`", true); press("v"); gd();
    expect(last("open_path")).toMatchObject({ path: "mesh.go", line: 142, expect: "Name" });
  });
  it("posts a valid Visual selection", () => {
    const { gd, press } = setup("Name"); press("v"); press("v");
    for (let i = 1; i < 4; i++) press("l");
    gd(); expect(last("editor_symbol")).toMatchObject({ name: "Name" });
  });
  it("reads only the caret's hard line for a citation", () => {
    const { gd, press } = setup("Name\nmesh.go:142"); press("v"); gd();
    expect(last("editor_symbol")).toMatchObject({ name: "Name" });
    expect(last("editor_symbol")).not.toHaveProperty("cited");
  });
  it("a newer name gd discards the older reply and its timer", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    const older = last("editor_symbol")!.request_id;
    press("v"); gd();
    dispatch({ kind: "editor_symbol_result", requestId: older, matches: [match("old.rs", 1)] });
    expect(last("open_path")).toBeUndefined();
    answer([match("new.rs", 2)]);
    expect(last("open_path")).toMatchObject({ path: "new.rs", line: 2 });
    act(() => vi.advanceTimersByTime(4000));
    expect(container.textContent).not.toContain("gd: no answer in 3 s");
  });
  it("a direct path gd also discards an older name lookup", () => {
    const { gd, press } = setup("Name at a.rs:3"); press("v"); gd();
    const older = last("editor_symbol")!.request_id;
    gd();
    const opened = posted.filter((p) => p.type === "open_path").length;
    dispatch({ kind: "editor_symbol_result", requestId: older, matches: [match("old.rs", 1)] });
    expect(posted.filter((p) => p.type === "open_path")).toHaveLength(opened);
  });
  it("gf drops a pending name gd without moving the interaction", () => {
    const { gd, press, container } = setup("Name at a.rs:3"); press("v"); gd();
    const older = last("editor_symbol")!.request_id;
    const mirrors = posted.filter((p) => p.type === "panel_keys").length;
    press("g"); press("f");
    expect(last("open_path")).toMatchObject({ path: "a.rs", line: 3 });
    expect(posted.filter((p) => p.type === "panel_keys")).toHaveLength(mirrors);
    const opened = posted.filter((p) => p.type === "open_path").length;
    dispatch({ kind: "editor_symbol_result", requestId: older, matches: [match("old.rs", 1)] });
    expect(posted.filter((p) => p.type === "open_path")).toHaveLength(opened);
    act(() => vi.advanceTimersByTime(4000));
    expect(container.textContent).not.toContain("gd: no answer in 3 s");
  });
  it("opens one symbol match via gf", () => {
    const { gd, press } = setup("Name"); press("v"); gd();
    answer([match("src/a.rs", 7)]);
    expect(last("open_path")).toMatchObject({ path: "src/a.rs", line: 7 });
  });
  it("chooses the match nearest the cited line in the cited file", () => {
    const { gd, press } = setup("Name at mesh.go:142"); press("v"); gd();
    answer([match("other.go", 142), match("mesh.go", 12), match("mesh.go", 140)]);
    expect(last("open_path")).toMatchObject({ path: "mesh.go", line: 140 });
  });
  it.each(["mesh.go", "pkg/mesh.go"])("uses the cited suffix %s to choose the nearest project-relative match", (cited) => {
    const { gd, press, container } = setup(`Name at ${cited}:142`); press("v"); gd();
    answer([match("other/mesh.go.bak", 142), match("pkg/mesh.go", 12), match("pkg/mesh.go", 140)]);
    expect(last("open_path")).toMatchObject({ path: "pkg/mesh.go", line: 140 });
    expect(container.querySelector(".path-pick")).toBeNull();
  });
  it("requires a slash boundary before the cited suffix", () => {
    const { gd, press, container } = setup("Name at mesh.go:142"); press("v"); gd();
    answer([match("pkg/notmesh.go", 142), match("other.go", 140)]);
    expect(last("open_path")).toBeUndefined();
    expect(container.querySelector(".path-pick")).not.toBeNull();
  });
  it("offers matches without a citation and labels text matches", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    answer([match("a.rs", 7), match("b.rs", 20, "text")]);
    expect(last("open_path")).toBeUndefined();
    expect(container.querySelector(".path-pick")?.textContent).toContain("b.rs:20 (text)");
    press("s"); expect(last("open_path")).toMatchObject({ path: "b.rs", line: 20 });
  });
  it.each([undefined, { kind: "no_client" }, { kind: "refused", message: "gd is not wired in this build" }])("falls back to the cited line on an empty/error answer: %j", (error) => {
    const { gd, press } = setup("Name at mesh.go:142"); press("v"); gd(); answer([], error);
    expect(last("open_path")).toMatchObject({ path: "mesh.go", line: 142, expect: "Name" });
  });
  it.each([
    [{ kind: "no_client" }, "gd: no language server attached for this project (open one of its files)"],
    [{ kind: "no_symbol", server: "gopls" }, "gd: gopls has no symbol Name"],
    [{ kind: "no_symbol" }, "gd: the language server has no symbol Name"],
    [{ kind: "indexing", server: "rust-analyzer" }, "gd: rust-analyzer is still indexing — try again"],
    [{ kind: "timeout" }, "gd: no answer in 3 s"],
    [{ kind: "refused", message: "gd is not wired in this build" }, "gd is not wired in this build"],
  ])("flashes %j", (error, message) => {
    const { gd, press, container } = setup("Name"); press("v"); gd(); answer([], error);
    expect(container.textContent).toContain(message); expect(last("open_path")).toBeUndefined();
  });
  it("refuses an invalid Visual selection without posting", () => {
    const { gd, press, container } = setup("bad name"); press("v"); press("v");
    for (let i = 1; i < 8; i++) press("l");
    gd();
    expect(last("editor_symbol")).toBeUndefined();
    expect(container.textContent).toContain("gd: bad name is not a name gd can look up");
  });
  it("times out at four seconds and ignores a late answer", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    act(() => vi.advanceTimersByTime(3999)); expect(container.textContent).not.toContain("gd: no answer");
    act(() => vi.advanceTimersByTime(1)); expect(container.textContent).toContain("gd: no answer in 3 s");
    answer([match("a.rs", 1)]); expect(last("open_path")).toBeUndefined();
  });
  it("shows the worker's advisory from its own envelope, and the editor's answer still settles the request", () => {
    const { gd, container } = setup("a.rs:1", true); gd();
    const requestId = last("open_path")!.request_id;
    dispatch({ kind: "open_path_note", requestId, note: 5 });
    expect(container.textContent).not.toContain("no longer has");
    dispatch({ kind: "open_path_note", requestId, note: "a.rs:1 no longer has Name (the file changed since the reply)" });
    expect(container.textContent).toContain("a.rs:1 no longer has Name");
    dispatch({ kind: "command_result", requestId, ok: false, error: "editor unavailable" });
    expect(container.textContent).toContain("editor unavailable");
  });
  it.each(["README.md", "App.tsx", "Cargo.toml"])("opens a bare file name %s under the caret as gf does", (file) => {
    const { gd, press } = setup(`${file} is the one`); press("v"); gd();
    expect(last("open_path")).toMatchObject({ path: file });
    expect(last("editor_symbol")).toBeUndefined();
  });
  it("a lookup the host refused before its arm ends there: no four-second flash after it", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    const requestId = last("editor_symbol")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "tab 1 is not shown in this page" });
    expect(container.textContent).toContain("gd: tab 1 is not shown in this page");
    act(() => vi.advanceTimersByTime(5000));
    expect(container.textContent).not.toContain("gd: no answer in 3 s");
    answer([match("a.rs", 1)]); expect(last("open_path")).toBeUndefined();
  });
  it.each(["gf", "gd"])("%s's open carries the interaction its mirror last said, and leaving BROWSE moves it on", (verb) => {
    const { gd, press } = setup(verb === "gf" ? "a.rs:7" : "Name");
    if (verb === "gd") { press("v"); gd(); answer([match("a.rs", 7)]); }
    else { press("g"); press("f"); }
    const carried = last("open_path")!.interaction as number;
    expect(carried).toBe(last("panel_keys")!.interaction);
    act(() => vi.advanceTimersByTime(1000));
    press("i");
    expect(last("panel_keys")).toMatchObject({ mode: "input" });
    expect(last("panel_keys")!.interaction as number).toBeGreaterThan(carried);
  });
  it.each(["typing", "focus"])("after an open, %s moves the interaction on once", (how) => {
    const { press, container } = setup("a.rs:7");
    press("g"); press("f");
    const carried = last("open_path")!.interaction as number;
    const before = posted.filter((p) => p.type === "panel_keys").length;
    const away = () => how === "typing"
      ? fireEvent.input(container.querySelector(".agent-ui-conversation")!)
      : dispatch({ kind: "pane_focus", focused: false });
    away(); away();
    const mirrors = posted.filter((p) => p.type === "panel_keys").slice(before);
    expect(mirrors).toHaveLength(1);
    expect(mirrors[0]!.interaction as number).toBeGreaterThan(carried);
  });
  it("typing with no open out moves nothing", () => {
    const { container } = setup("a.rs:7");
    const before = posted.filter((p) => p.type === "panel_keys").length;
    fireEvent.input(container.querySelector(".agent-ui-conversation")!);
    expect(posted.filter((p) => p.type === "panel_keys")).toHaveLength(before);
  });
  it("a late single match while typing in the box opens nothing and names the place", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    expect(last("editor_symbol")).toMatchObject({ name: "Name" });
    act(() => vi.advanceTimersByTime(1000));
    press("i");
    const box = container.querySelector<HTMLTextAreaElement>("textarea")!;
    expect(document.activeElement).toBe(box);
    fireEvent.keyDown(box, { key: "x" }); fireEvent.keyUp(box, { key: "x" });
    answer([match("src/a.rs", 7)]);
    expect(last("open_path")).toBeUndefined();
    expect(container.querySelector(".band-message")?.textContent).toBe("gd: src/a.rs:7 — gd again to open");
    expect(document.activeElement).toBe(box);
  });
  it("a late cited fallback while typing in the box opens nothing either", () => {
    const { gd, press, container } = setup("Name at mesh.go:142"); press("v"); gd();
    act(() => vi.advanceTimersByTime(1000));
    press("i");
    expect(document.activeElement?.tagName).toBe("TEXTAREA");
    answer([], { kind: "no_client" });
    expect(last("open_path")).toBeUndefined();
    expect(container.querySelector(".band-message")?.textContent).toBe("gd: mesh.go:142 — gd again to open");
  });
  it("a late single match after the pane lost the keys opens nothing", () => {
    const { gd, press } = setup("Name"); press("v"); gd();
    dispatch({ kind: "pane_focus", focused: false });
    answer([match("src/a.rs", 7)]);
    expect(last("open_path")).toBeUndefined();
  });
  it("ignores symbol answers after switching tabs", () => {
    const { gd, press } = setup("Name"); press("v"); gd();
    dispatch({ kind: "tabs", tabs: [TAB, { ...TAB, id: 2, number: 2 }], active: 2 });
    answer([match("a.rs", 1)]); expect(last("open_path")).toBeUndefined();
  });
});

describe("a lookup answered while card mode holds the keys", () => {
  /** A card waiting in the same tab, then card mode on it by `Alt+n` from BROWSE. */
  function cardMode(container: HTMLElement) {
    const input = { command: "rm build" };
    dispatch({ kind: "events", tab: 1, fromRevision: 2, throughRevision: 5, events: [
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input },
    ] });
    act(() => vi.advanceTimersByTime(1000));
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    if (document.activeElement !== root) act(() => root.focus());
    fireEvent.keyDown(document.activeElement ?? root, { key: "n", altKey: true, code: "KeyN" });
    expect(container.querySelector(".permission-card-mode"), "card mode holds the keys").not.toBeNull();
  }
  const bandText = (container: HTMLElement) => container.querySelector(".band-message")?.textContent ?? null;

  it("a gd name with several definitions opens no pick over the card, and card mode stays", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    expect(last("editor_symbol")).toMatchObject({ name: "Name" });
    cardMode(container);
    const opens = posted.filter((p) => p.type === "open_path").length;
    answer([match("src/a.rs", 3), match("src/b.rs", 9)]);
    expect(container.querySelector(".path-pick")).toBeNull();
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    expect(bandText(container)).toBe("gd: press Esc to leave the card first");
    expect(posted.filter((p) => p.type === "open_path")).toHaveLength(opens);
  });

  it("a gd name with one definition opens nothing over the card, and card mode stays", () => {
    const { gd, press, container } = setup("Name"); press("v"); gd();
    cardMode(container);
    answer([match("src/a.rs", 3)]);
    expect(last("open_path")).toBeUndefined();
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    expect(bandText(container)).toBe("gd: src/a.rs:3 — gd again to open");
  });

  it("gf's several matching files open no pick over the card, and card mode stays", () => {
    const { press, container } = setup("see a.go", true); press("g"); press("f");
    const request = last("open_path")!;
    expect(request).toMatchObject({ path: "a.go" });
    cardMode(container);
    dispatch({ kind: "path_choices", requestId: request.request_id, tab: 1, paths: [{ path: "src/a.go", line: null }, { path: "test/a.go", line: null }] });
    expect(container.querySelector(".path-pick")).toBeNull();
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    expect(bandText(container)).toBe("gf: press Esc to leave the card first");
    expect(posted.filter((p) => p.type === "open_path")).toHaveLength(1);
  });
});
