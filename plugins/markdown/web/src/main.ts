import "katex/dist/katex.css";
import "./style/markdown.css";
import "./style/hljs.css";
import "./style/alerts.css";
import "./style/chrome.css";
import { hasPosition, onRestored, onSave, type MarkdownState } from "./handoff";
import { kit } from "./kit";
import { bridge, follow, hello, isEditorClosed, isEditorText, isEditorView, readFontSize, readSettings } from "./protocol";
import { createMermaid } from "./render/mermaid";
import { trace } from "./trace";
import { yankCurrentBlock } from "./view/clipboard";
import { installKeys } from "./view/keys";
import { installLinks } from "./view/links";
import { Preview } from "./view/preview";
import { applyFontSize } from "./view/text-size";

function hasKeysOf(data: unknown): boolean | undefined {
  return data !== null && typeof data === "object" && typeof (data as { has_keys?: unknown }).has_keys === "boolean"
    ? (data as { has_keys: boolean }).has_keys : undefined;
}

function themeVarsOf(data: unknown): Record<string, string> | null {
  const vars = data !== null && typeof data === "object" ? (data as { vars?: unknown }).vars : undefined;
  if (vars === null || typeof vars !== "object" || Array.isArray(vars)) return null;
  const out: Record<string, string> = {};
  for (const [name, value] of Object.entries(vars as Record<string, unknown>)) if (typeof value === "string") out[name] = value;
  return out;
}

async function start(): Promise<void> {
  trace("start", {});
  const b = bridge();
  const root = document.getElementById("mdp-root");
  if (!root) throw new Error("the page has no #mdp-root");
  // The host says where the keys are right after hello's reply, before the preview exists.
  let keys = false;
  let preview: Preview | null = null;
  b.on("focus", (data) => {
    const value = hasKeysOf(data);
    if (value === undefined) return;
    keys = value;
    preview?.onFocus(value);
  });
  const h = await hello(b);
  const settings = readSettings(h);
  applyFontSize(document.documentElement, readFontSize(h));
  const mermaid = createMermaid();
  mermaid.setTheme(h.theme.vars);
  const p = new Preview(root, b, settings, mermaid);
  preview = p;
  p.onFocus(keys);
  const post = (verb: string, payload: unknown) => {
    void b.call(verb, payload).catch((error: unknown) => console.error(error));
  };
  const searchInput = () => document.querySelector<HTMLInputElement>("#eitri-keys-find input");
  let restored: MarkdownState | null = null;
  let search: string | null = null;
  let ready: (() => void) | null = null;
  b.on("host.save", () => onSave(() => {
    const draft = searchInput()?.value ?? search;
    // No document shown and none waiting: no position, so the successor is ready without a render.
    if (!restored && p.shownBuf() === null) return { v: 1, topLine: 0, fraction: 0, search: draft, noDocument: true };
    const line = restored ? restored.topLine + restored.fraction : p.readingLine();
    const topLine = Math.floor(line);
    return { v: 1, topLine, fraction: line - topLine, search: draft };
  }, post));
  b.on("editor.view", (data) => {
    if (!isEditorView(data)) return;
    // The first view must target the saved line before its text renders, or sync would replace it immediately.
    p.onView(restored ? { ...data, top: restored.topLine + 1, top_fraction: restored.fraction } : data);
  });
  b.on("editor.text", (data) => { if (isEditorText(data)) p.onText(data); });
  b.on("editor.closed", (data) => { if (isEditorClosed(data)) p.onClosed(data); });
  // A new colourscheme: the CSS follows by itself; diagrams carry their colours inside, so they draw again.
  b.on("theme", (data) => {
    const vars = themeVarsOf(data);
    if (!vars) return;
    mermaid.setTheme(vars);
    p.onTextSize();
    p.rerender();
  });
  installLinks(p, root);
  // The page's reading keys. A refused table (the host checks every key) leaves the preview readable by pointer.
  await installKeys(p, {
    timeoutlen: h.timeoutlen,
    prefix: h.prefix,
    settings: h.keys_settings,
    run: (action) => {
      if (action !== "block.yank") return false;
      void yankCurrentBlock(p);
      return true;
    },
  }).catch((error: unknown) => console.error(error));
  onRestored((h as typeof h & { restored?: unknown }).restored, (state) => {
    // A state without a position waits for nothing: the search draft is all it carries.
    restored = hasPosition(state) ? state : null;
    search = state.search;
    if (search !== null) {
      kit()?.findSeed(search);
      const input = searchInput();
      if (input) input.value = search;
      else {
        // The kit creates its prompt lazily and clears it on opening; refill the draft when it first receives focus.
        const fill = (event: FocusEvent) => {
          const input = searchInput();
          if (!input || event.target !== input) return;
          input.value = search ?? "";
          document.removeEventListener("focusin", fill);
        };
        document.addEventListener("focusin", fill);
      }
    }
  }, (cb) => {
    if (restored) ready = cb;
    else requestAnimationFrame(cb);
  }, post);
  // The source map exists only after rendering. Say ready after that position has had a frame to draw.
  const offRestored = p.afterUpdate((why) => {
    if (!restored || (why !== "render" && why !== "notice")) return;
    if (why === "render") p.showLine(restored.topLine + restored.fraction);
    restored = null;
    offRestored();
    if (ready) requestAnimationFrame(ready);
    ready = null;
  });
  await follow(b);
}

start().catch((error: unknown) => console.error(error));
