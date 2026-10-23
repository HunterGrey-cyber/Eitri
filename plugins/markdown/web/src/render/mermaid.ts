import { trace } from "../trace";
import { NONCE_ATTR } from "./env";
import { sanitizeSvg } from "./sanitize";

/** What the pipeline asks of the diagram renderer: `fill` puts already drawn diagrams into a fresh fragment and
 *  returns how many, `renderPending` draws the ones still waiting once the fragment is in the page, and `setTheme`
 *  takes the page's colours for the next drawing. */
export interface MermaidRunner {
  fill(root: ParentNode): number;
  renderPending(root: ParentNode, onDone: () => void): void;
  setTheme(vars: Record<string, string>): void;
}

/** The part of the diagram library the runner uses. */
export interface MermaidApi {
  initialize(config: Record<string, unknown>): void;
  render(id: string, text: string): Promise<{ svg: string }>;
}

// Held in a constant so neither the type checker nor the bundler tries to resolve it: at run time it resolves
// against the page script's own URL, where the build puts the diagram library as a file of its own.
const MERMAID_URL = "./mermaid.js";

const NEEDS_LATER = "This diagram needs a later version of the preview.";

export const MERMAID_CONFIG = {
  // Diagrams are drawn only when the page asks, never by the library scanning the page on its own.
  startOnLoad: false,
  // No script, click handler or link from a diagram's text runs or is kept.
  securityLevel: "strict",
  // Labels as SVG text rather than HTML inside `foreignObject`, which the sanitiser would remove anyway.
  htmlLabels: false,
  flowchart: { htmlLabels: false },
  // A cap on the text one diagram may hold, so a huge block cannot stall the page.
  maxTextSize: 50000,
  // The one theme whose colours all come from the variables below, which follow the editor's.
  theme: "base",
  // A failed diagram is shown by the page; the library must not insert its own error drawing into the document.
  suppressErrorRendering: true,
} as const;

const THEME: readonly [string, string][] = [
  ["background", "--nv-bg"], ["textColor", "--nv-fg"], ["lineColor", "--nv-fg"],
  ["primaryColor", "--nv-surface"], ["primaryTextColor", "--nv-surface-fg"], ["primaryBorderColor", "--nv-border"],
  ["mainBkg", "--nv-surface"], ["nodeBorder", "--nv-border"],
  ["secondaryColor", "--nv-tint"], ["secondaryTextColor", "--nv-tint-fg"], ["secondaryBorderColor", "--nv-border"],
  ["tertiaryColor", "--nv-cursorline"], ["tertiaryTextColor", "--nv-fg"], ["tertiaryBorderColor", "--nv-border"],
  ["clusterBkg", "--nv-tint"], ["clusterBorder", "--nv-border"],
  ["noteBkgColor", "--nv-surface"], ["noteTextColor", "--nv-surface-fg"], ["noteBorderColor", "--nv-border"],
  ["errorBkgColor", "--nv-error-tint"], ["errorTextColor", "--nv-error-tint-fg"],
  ["titleColor", "--nv-fg"], ["edgeLabelBackground", "--nv-bg"],
];

// Pie slices and colour scales take the syntax colours, which a theme already keeps apart from each other.
const PALETTE = ["--nv-syn-function", "--nv-syn-string", "--nv-syn-keyword", "--nv-syn-type", "--nv-syn-number",
  "--nv-syn-constant"];

/** The diagram theme's variables from the page's live `--nv-*` values; a value the page lacks is left to the
 *  library's own default. */
export function themeVariables(vars: Record<string, string>): Record<string, string | boolean> {
  const out: Record<string, string | boolean> = { darkMode: vars["--nv-color-scheme"] !== "light" };
  for (const [key, name] of THEME) if (vars[name]) out[key] = vars[name];
  PALETTE.forEach((name, i) => {
    if (!vars[name]) return;
    out[`pie${i + 1}`] = vars[name];
    out[`cScale${i}`] = vars[name];
  });
  if (vars["--nv-font-prose"]) out.fontFamily = vars["--nv-font-prose"];
  return out;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string);
}

/** A diagram waiting to be drawn: its source as text in a renderer-marked element, so until it is drawn (or when it
 *  cannot be) the reader still sees what the diagram says. */
export function mermaidPlaceholder(source: string, nonce: string): string {
  return `<div class="mdp-diagram mdp-diagram-pending" data-eitri-mermaid="" ${NONCE_ATTR}="${escapeHtml(nonce)}">` +
    `${escapeHtml(source)}</div>`;
}

// The library rejects a bad diagram with a plain object carrying `message` or `str`, not always with an `Error`.
function messageOf(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (e && typeof e === "object") {
    const shaped = e as { message?: unknown; str?: unknown };
    if (typeof shaped.message === "string" && shaped.message) return shaped.message;
    if (typeof shaped.str === "string" && shaped.str) return shaped.str;
  }
  return String(e);
}

// A page whose policy forbids evaluating strings refuses a library that needs it. That is not the diagram's fault,
// so the reader is told so rather than shown an error about their text.
function evalRefused(e: unknown): boolean {
  return (typeof EvalError !== "undefined" && e instanceof EvalError) || /unsafe-eval/.test(messageOf(e));
}

// Some WebKit builds lack `requestIdleCallback`; a short timeout still yields to input and painting.
function idle(): Promise<void> {
  return new Promise((resolve) => {
    const w = window as unknown as { requestIdleCallback?: (f: () => void) => void };
    if (w.requestIdleCallback) w.requestIdleCallback(() => resolve());
    else setTimeout(resolve, 1);
  });
}

type Outcome = { svg: string } | { error: string; later: boolean };

/** How many drawn outcomes the runner keeps, and how many characters of SVG and message they may hold together.
 *  Every edit inside a diagram is a new source, so without a bound a long editing session kept every revision's
 *  drawing for as long as the page lived. The bound counts only drawings of sources the document no longer holds:
 *  the newest outcome and every diagram of the document last filled are always kept, however many or large, since
 *  evicting one of those would draw it again on the next edit anywhere else. */
export const CACHE_ENTRIES = 64;
export const CACHE_CHARS = 4 * 1024 * 1024;

function charsOf(outcome: Outcome): number {
  return "svg" in outcome ? outcome.svg.length : outcome.error.length;
}

const SELECTOR = "[data-eitri-mermaid]";
const PENDING = "mdp-diagram-pending";

async function loadMermaid(): Promise<MermaidApi> {
  return ((await import(MERMAID_URL)) as { default: MermaidApi }).default;
}

/** The diagram runner. The library is loaded on the first diagram that needs drawing and never for a document
 *  without one; diagrams are drawn one at a time, each on an idle moment, so a page of them never freezes the
 *  view; and the outcomes of the document's diagrams, and those used last before them (`CACHE_ENTRIES`,
 *  `CACHE_CHARS`), are kept by source for the current theme, so an edit elsewhere in the document redraws nothing. */
export function createMermaid(load: () => Promise<MermaidApi> = loadMermaid): MermaidRunner {
  let api: Promise<MermaidApi> | null = null;
  let generation = 0;
  let vars = themeVariables({});
  let configured = -1;
  // In the order last used, oldest first.
  const done = new Map<string, Outcome>();
  let doneChars = 0;
  // The sources of the document last filled, whose outcomes the next render needs again.
  let inDocument = new Set<string>();
  // A diagram's last drawn height by its position in the document, so the box that waits for an edited diagram
  // keeps the text below it from jumping.
  const heights = new Map<number, number>();
  let latest: { root: ParentNode; onDone: () => void } | null = null;
  let running = false;
  let counter = 0;

  const keyOf = (gen: number, source: string) => `${gen}\u0000${source}`;
  const sourceOfKey = (key: string) => key.slice(key.indexOf("\u0000") + 1);

  function recall(key: string): Outcome | undefined {
    const outcome = done.get(key);
    if (outcome) {
      done.delete(key);
      done.set(key, outcome);
    }
    return outcome;
  }

  function remember(key: string, outcome: Outcome): void {
    const old = done.get(key);
    if (old) {
      done.delete(key);
      doneChars -= charsOf(old);
    }
    done.set(key, outcome);
    doneChars += charsOf(outcome);
    for (const [k, o] of done) {
      if (done.size <= CACHE_ENTRIES && doneChars <= CACHE_CHARS) break;
      if (k === key || inDocument.has(sourceOfKey(k))) continue;
      done.delete(k);
      doneChars -= charsOf(o);
    }
  }

  function draw(el: HTMLElement, key: string, outcome: Outcome): void {
    const source = el.getAttribute("data-mdp-source") ?? "";
    el.classList.remove(PENDING);
    el.setAttribute("data-mdp-key", key);
    el.style.minHeight = "";
    if ("svg" in outcome) {
      el.classList.remove("mdp-box");
      el.replaceChildren(sanitizeSvg(outcome.svg));
      return;
    }
    el.classList.add("mdp-box");
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    code.textContent = source;
    pre.append(code);
    const note = document.createElement("p");
    note.textContent = outcome.later ? NEEDS_LATER : outcome.error;
    el.replaceChildren(pre, note);
  }

  // The library, configured for the current theme, and the theme generation it is configured for.
  async function mermaid(): Promise<[MermaidApi, number]> {
    if (!api) {
      api = load().then(
        (m) => {
          trace("diagram", { state: "loaded" });
          return m;
        },
        (e: unknown) => {
          // A failed load is that diagram's error; the next diagram tries again.
          api = null;
          throw e;
        },
      );
    }
    const m = await api;
    if (configured !== generation) {
      m.initialize({ ...MERMAID_CONFIG, themeVariables: vars });
      configured = generation;
    }
    return [m, configured];
  }

  async function drain(): Promise<void> {
    running = true;
    try {
      while (latest) {
        const { root } = latest;
        const el = root.querySelector<HTMLElement>(`${SELECTOR}.${PENDING}[data-mdp-source]`);
        if (!el) {
          latest = null;
          break;
        }
        await idle();
        // The page may have rendered again while this waited; then the newest document's diagrams come first.
        if (!latest || latest.root !== root || !root.contains(el) || !el.classList.contains(PENDING)) continue;
        const source = el.getAttribute("data-mdp-source") ?? "";
        let gen = generation;
        let key = keyOf(gen, source);
        let outcome = recall(key);
        if (!outcome) {
          try {
            const [m, configuredFor] = await mermaid();
            gen = configuredFor;
            outcome = { svg: (await m.render(`mdp-diagram-${++counter}`, source)).svg };
            trace("diagram", { state: "rendered" });
          } catch (e) {
            outcome = { error: messageOf(e), later: evalRefused(e) };
            trace("diagram", { state: outcome.later ? "eval-refused" : "error" });
          }
          // Under the theme it was drawn in, which a theme change during the render may have moved past; a drawing
          // in a theme already left is never looked up again, so it is not kept.
          key = keyOf(gen, source);
          if (gen === generation) remember(key, outcome);
        }
        // A drawing in colours the page has since left is not shown: the diagram stays waiting, and the next turn of
        // this loop draws it again in the current theme.
        if (gen !== generation) continue;
        const current = latest;
        if (!current || !current.root.contains(el)) continue;
        if (el.getAttribute("data-mdp-source") === source && el.classList.contains(PENDING)) {
          draw(el, key, outcome);
          const position = [...current.root.querySelectorAll(SELECTOR)].indexOf(el);
          const height = el.getBoundingClientRect().height;
          if (position >= 0 && height > 0) heights.set(position, height);
        }
        // The diagram changed the page's height, so the view re-anchors.
        current.onDone();
      }
    } finally {
      running = false;
    }
  }

  return {
    fill(root) {
      let filled = 0;
      inDocument = new Set<string>();
      root.querySelectorAll<HTMLElement>(SELECTOR).forEach((el, i) => {
        const source = el.getAttribute("data-mdp-source") ?? el.textContent ?? "";
        el.setAttribute("data-mdp-source", source);
        inDocument.add(source);
        const key = keyOf(generation, source);
        const outcome = recall(key);
        if (outcome) {
          draw(el, key, outcome);
          filled++;
          return;
        }
        el.classList.add(PENDING);
        el.removeAttribute("data-mdp-key");
        const height = heights.get(i);
        el.style.minHeight = height ? `${height}px` : "";
        const label = document.createElement("span");
        label.className = "mdp-box-reason";
        label.textContent = "rendering diagram";
        el.replaceChildren(label);
      });
      return filled;
    },
    renderPending(root, onDone) {
      latest = { root, onDone };
      if (!running) void drain();
    },
    setTheme(next) {
      const v = themeVariables(next);
      if (JSON.stringify(v) === JSON.stringify(vars)) return;
      vars = v;
      generation++;
      // Every kept drawing is in the theme just left.
      done.clear();
      doneChars = 0;
    },
  };
}
