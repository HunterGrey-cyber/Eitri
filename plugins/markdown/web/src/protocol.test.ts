import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import {
  bridge, follow, hello, isEditorClosed, isEditorText, isEditorView, isHello, jump, openLink, readSettings, reveal,
  type Bridge,
} from "./protocol";

// A path, not `new URL(..., import.meta.url)`: vite rewrites that pattern into a dev-server address.
const FIXTURES = resolve(dirname(fileURLToPath(import.meta.url)), "../../../../core/src/plugin/fixtures");
const fixture = (name: string) => JSON.parse(readFileSync(resolve(FIXTURES, name), "utf8"));

function fakeBridge(replies: Record<string, unknown>) {
  const calls: { verb: string; args: unknown }[] = [];
  const b: Bridge = {
    call: async (verb, args) => { calls.push({ verb, args }); return replies[verb]; },
    on: () => () => {},
  };
  return { b, calls };
}

afterEach(() => { delete (window as { eitriPlugin?: unknown }).eitriPlugin; });

describe("hello", () => {
  it("reads the settings member of the shared fixture", async () => {
    const result = fixture("rep-hello-settings.json").result;
    expect(isHello(result)).toBe(true);
    const { b } = fakeBridge({ "host.hello": result });
    const h = await hello(b);
    expect(readSettings(h)).toEqual({ auto: false, remote_images: "https", sync: "both" });
    expect(h.theme.vars["--nv-link"]).toMatch(/^#[0-9a-f]{6}$/);
  });
  it("gives the defaults for a plugin with no bool or choice setting", async () => {
    const { b } = fakeBridge({ "host.hello": fixture("rep-hello.json").result });
    expect(readSettings(await hello(b))).toEqual({ auto: false, remote_images: "https", sync: "both" });
  });
  it("takes each declared value and falls back per setting on a wrong one", async () => {
    const base = fixture("rep-hello.json").result;
    const { b } = fakeBridge({ "host.hello": { ...base, settings: { auto: true, remote_images: "off", sync: "editor" } } });
    expect(readSettings(await hello(b))).toEqual({ auto: true, remote_images: "off", sync: "editor" });
    const bad = fakeBridge({ "host.hello": { ...base, settings: { auto: "yes", remote_images: "http", sync: 3 } } });
    expect(readSettings(await hello(bad.b))).toEqual({ auto: false, remote_images: "https", sync: "both" });
  });
  it("hands the kit's settings through as the host sent them", async () => {
    const result = fixture("rep-hello.json").result;
    const { b } = fakeBridge({ "host.hello": result });
    expect((await hello(b)).keys_settings).toEqual(result.keys_settings);
  });
  it("refuses a reply that is not a hello", async () => {
    const { b } = fakeBridge({ "host.hello": { host: "other" } });
    await expect(hello(b)).rejects.toThrow("the host's hello reply is malformed");
  });
});

describe("events", () => {
  it("accepts the shared view, text and closed fixtures", () => {
    const view = fixture("ev-editor-view.json");
    expect(view.event).toBe("editor.view");
    expect(isEditorView(view.data)).toBe(true);
    expect(view.data.doc).toEqual({
      root: "eitri-plugin://markdown/.eitri/doc/4/",
      base: "eitri-plugin://markdown/.eitri/doc/4/docs/",
    });
    expect(view.data.scrolloff).toBe(4);
    expect([view.data.rows, view.data.cursor_row]).toEqual([46, 7]);
    const echo = fixture("ev-editor-view-reveal.json").data;
    expect(isEditorView(echo)).toBe(true);
    expect([echo.cause, echo.token, echo.doc, echo.scrolloff, echo.rows, echo.cursor_row])
      .toEqual(["reveal", 7, null, undefined, undefined, undefined]);
    const text = fixture("ev-editor-text.json").data;
    expect(isEditorText(text) && "text" in text).toBe(true);
    const large = fixture("ev-editor-text-too-large.json").data;
    expect(isEditorText(large)).toBe(true);
    expect(large.too_large).toEqual({ bytes: 5452595, lines: 61000, max_bytes: 4194304, max_lines: 50000 });
    expect(isEditorClosed(fixture("ev-editor-closed.json").data)).toBe(true);
  });
  it("refuses malformed events", () => {
    const view = fixture("ev-editor-view.json").data;
    const { cursor: _cursor, ...noCursor } = view;
    expect(isEditorView(noCursor)).toBe(false);
    expect(isEditorView({ ...view, cause: "other" })).toBe(false);
    expect(isEditorView({ ...view, top_fraction: 1.5 })).toBe(false);
    expect(isEditorView({ ...view, doc: { root: 1, base: "x" } })).toBe(false);
    expect(isEditorView({ ...view, scrolloff: -1 })).toBe(false);
    expect(isEditorView({ ...view, scrolloff: 1.5 })).toBe(false);
    expect(isEditorView({ ...view, scrolloff: "4" })).toBe(false);
    for (const key of ["rows", "cursor_row"]) {
      for (const bad of [-1, 1.5, "7", null]) expect(isEditorView({ ...view, [key]: bad })).toBe(false);
    }
    const text = fixture("ev-editor-text.json").data;
    expect(isEditorText({ ...text, too_large: { bytes: 1, lines: 1, max_bytes: 1, max_lines: 1 } })).toBe(false);
    const { text: _text, ...neither } = text;
    expect(isEditorText(neither)).toBe(false);
    expect(isEditorClosed({ buf: 3, reason: "other" })).toBe(false);
  });
});

describe("verbs", () => {
  it("sends each verb's arguments in the shape the shared requests have and reads the shared replies", async () => {
    const { b, calls } = fakeBridge({
      "editor.follow": fixture("rep-editor-follow.json").result,
      "editor.reveal": fixture("rep-editor-reveal.json").result,
      "editor.jump": fixture("rep-editor-jump.json").result,
      "editor.open_link": fixture("rep-editor-open-link.json").result,
    });
    expect(await follow(b)).toEqual({ filetypes: ["markdown"] });
    expect(await reveal(b, { buf: 3, line: 120, token: 7 })).toEqual({ applied: true });
    expect(await reveal(b, { buf: 3, line: 120, token: 8, cursor: 124 })).toEqual({ applied: true });
    expect(await reveal(b, { buf: 3, line: 120, token: 8, cursor: 124, row: 9 })).toEqual({ applied: true });
    expect(await jump(b, { buf: 3, line: 42, col: 0, focus: true })).toEqual(fixture("rep-editor-jump.json").result);
    expect(await openLink(b, { buf: 3, target: "../docs/setup.md#L10" })).toEqual({ how: "edit", line: 10 });
    expect(calls).toEqual([
      { verb: "editor.follow", args: fixture("req-editor-follow.json").args },
      { verb: "editor.reveal", args: fixture("req-editor-reveal.json").args },
      { verb: "editor.reveal", args: fixture("req-editor-reveal-cursor.json").args },
      { verb: "editor.reveal", args: fixture("req-editor-reveal-row.json").args },
      { verb: "editor.jump", args: fixture("req-editor-jump.json").args },
      { verb: "editor.open_link", args: fixture("req-editor-open-link.json").args },
    ]);
  });
  it("refuses a malformed reply", async () => {
    const { b } = fakeBridge({ "editor.reveal": {}, "editor.open_link": { how: "tab", line: 1 } });
    await expect(reveal(b, { buf: 1, line: 1, token: 1 })).rejects.toThrow("malformed");
    await expect(openLink(b, { buf: 1, target: "x.md" })).rejects.toThrow("malformed");
  });
  it("is not connected without the host bridge", () => {
    expect(() => bridge()).toThrow("not connected to Eitri");
    const b = fakeBridge({}).b;
    (window as { eitriPlugin?: unknown }).eitriPlugin = b;
    expect(bridge()).toBe(b);
  });
});
