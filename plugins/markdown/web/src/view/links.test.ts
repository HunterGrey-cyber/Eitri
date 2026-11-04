import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { installLinks } from "./links";
import { Preview } from "./preview";

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
let layout: FakeLayout;
let preview: Preview;
let root: HTMLElement;
let calls: { verb: string; args: unknown }[];
let reply: () => Promise<unknown>;
let remove: () => void;
const kit = { version: 2, install: vi.fn(), setCurrent: vi.fn(), message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []) };

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root);
  // The reading reserve is at least five lines of the page's text; these short fake documents are written around a
  // 44 px reserve, so their page text is 8.8 px a line (the floor itself: preview.test.ts).
  document.getElementById("mdp-root")!.style.lineHeight = "8.8px";
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  kit.message.mockClear();
  calls = [];
  reply = async () => ({ how: "edit", line: 0 });
  const bridge: Bridge = { call: (verb, args) => { calls.push({ verb, args }); return reply(); }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  preview = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  remove = installLinks(preview, root);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => { remove(); preview.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks(); });

function show(body: string, buf = 1, tick = 1): void {
  const v: EditorView = { buf, name: `/p/${buf}.md`, filetype: "markdown", tick, current: true, line_count: 40, top: 1,
    top_fraction: 0, bottom: 20, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC };
  preview.onView(v);
  preview.onText({ buf, tick, name: `/p/${buf}.md`, text: body });
  vi.advanceTimersByTime(0);
}
const paras = (n: number) => Array.from({ length: n }, (_, i) => `p${i}`).join("\n\n");
function click(selector: string): MouseEvent {
  const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
  root.querySelector(selector)!.dispatchEvent(ev);
  return ev;
}

describe("links in the preview", () => {
  it("scroll to an anchor, prefixed as the sanitiser prefixes ids, and record a history entry", () => {
    show(`[go](#later)\n\n${Array.from({ length: 12 }, (_, i) => `p${i}`).join("\n\n")}\n\n## Later`);
    const ev = click('a[href="#later"]');
    expect(ev.defaultPrevented).toBe(true);
    expect(window.scrollY).toBe(layout.boxOf(root.querySelector("#user-content-later")!)!.top - 44);
    expect((history.state as { mdp?: boolean } | null)?.mdp).toBe(true);
  });

  it("open a local file in nvim as written, and scroll to a fragment that is not a line once it renders", async () => {
    show("[setup](docs/setup.md#install) [code](src/main.rs#L10)");
    click('a[href="docs/setup.md#install"]');
    expect(calls).toEqual([{ verb: "editor.open_link", args: { buf: 1, target: "docs/setup.md#install" } }]);
    await Promise.resolve();
    await Promise.resolve();
    show(`${Array.from({ length: 12 }, (_, i) => `p${i}`).join("\n\n")}\n\n## Install`, 2);
    expect(window.scrollY).toBe(layout.boxOf(root.querySelector("#user-content-install")!)!.top - 44);
  });

  it("scroll to a fragment of the document already shown at once, since opening it renders nothing new", async () => {
    const body = `[alias](alias.md#details) [self](./1.md#details)\n\n${paras(12)}\n\n## Details\n\n${paras(12)}`;
    show(body);
    const details = () => layout.boxOf(root.querySelector("#user-content-details")!)!.top - 44;
    // nvim's current file is the target (through another name here) and the preview shows nvim's current buffer.
    reply = async () => ({ how: "current", line: 1 });
    click('a[href="alias.md#details"]');
    await vi.waitFor(() => expect(window.scrollY).toBe(details()));
    expect((history.state as { mdp?: boolean } | null)?.mdp).toBe(true);
    window.scrollTo({ top: 0 });
    // The link names the shown file itself, whatever nvim did to open it.
    reply = async () => ({ how: "edit", line: 1 });
    click('a[href="./1.md#details"]');
    await vi.waitFor(() => expect(window.scrollY).toBe(details()));
  });

  it("keep a fragment for the opened file, not for a later edit of the document clicked in", async () => {
    const body = `[other](other.md#later)\n\n${paras(12)}\n\n## Later`;
    show(body);
    click('a[href="other.md#later"]');
    await Promise.resolve();
    await Promise.resolve();
    expect(window.scrollY).toBe(0);
    show(`${body}\n\nmore`, 1, 2);
    expect(window.scrollY).toBe(0);
    show(`${paras(14)}\n\n## Later`, 2);
    expect(window.scrollY).toBe(layout.boxOf(root.querySelector("#user-content-later")!)!.top - 44);
  });

  it("resolve a local link against the document shown, while another buffer's view waits for its text", async () => {
    show("[setup](docs/setup.md)");
    const v: EditorView = { buf: 2, name: "/q/2.md", filetype: "markdown", tick: 1, current: true, line_count: 40,
      top: 1, top_fraction: 0, bottom: 20, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC };
    preview.onView(v);
    click('a[href="docs/setup.md"]');
    expect(calls).toEqual([{ verb: "editor.open_link", args: { buf: 1, target: "docs/setup.md" } }]);
  });

  it("say why a link was refused", async () => {
    show("[a](../../x.md) [b](missing.md)");
    reply = async () => { throw Object.assign(new Error("outside"), { code: "outside_roots" }); };
    click('a[href="../../x.md"]');
    await vi.waitFor(() => expect(kit.message).toHaveBeenLastCalledWith("outside the project"));
    reply = async () => { throw Object.assign(new Error("gone"), { code: "io" }); };
    click('a[href="missing.md"]');
    await vi.waitFor(() => expect(kit.message).toHaveBeenLastCalledWith("no such file"));
  });

  it("leave an external link to its default click, and treat a raw HTML link like a Markdown one", () => {
    show('[web](https://example.org/) <a href="other.md">raw</a>');
    expect(click('a[href="https://example.org/"]').defaultPrevented).toBe(false);
    click('a[href="other.md"]');
    expect(calls.at(-1)).toEqual({ verb: "editor.open_link", args: { buf: 1, target: "other.md" } });
  });
});
