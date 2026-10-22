import "katex/dist/katex.css";
import "./style/markdown.css";
import "./style/hljs.css";
import "./style/alerts.css";
import "./style/chrome.css";
import { bridge, follow, hello, isEditorClosed, isEditorText, isEditorView, readSettings } from "./protocol";
import { createMermaid } from "./render/mermaid";
import { trace } from "./trace";
import { yankCurrentBlock } from "./view/clipboard";
import { installKeys } from "./view/keys";
import { installLinks } from "./view/links";
import { Preview } from "./view/preview";

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
  const mermaid = createMermaid();
  mermaid.setTheme(h.theme.vars);
  const p = new Preview(root, b, settings, mermaid);
  preview = p;
  p.onFocus(keys);
  b.on("editor.view", (data) => { if (isEditorView(data)) p.onView(data); });
  b.on("editor.text", (data) => { if (isEditorText(data)) p.onText(data); });
  b.on("editor.closed", (data) => { if (isEditorClosed(data)) p.onClosed(data); });
  // A new colourscheme: the CSS follows by itself; diagrams carry their colours inside, so they draw again.
  b.on("theme", (data) => {
    const vars = themeVarsOf(data);
    if (!vars) return;
    mermaid.setTheme(vars);
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
  await follow(b);
}

start().catch((error: unknown) => console.error(error));
