/* Measure the trust prompt's first paint in real WebKit after the shipped `dist/index.html` loads.
   Send `trust_prompt` through `__eitriDispatch` as Rust does. Stop the clock at the second animation frame
   after the overlay enters the document, when the page counts it as painted.
   Compare a small diff with one at the row budget; absolute timings depend on the environment. */

type BenchHandler = { postMessage: (message: string) => void };
const benchHandler = (window as unknown as { webkit: { messageHandlers: { eitriBench: BenchHandler } } }).webkit.messageHandlers.eitriBench;
const post = (message: unknown) => benchHandler.postMessage(JSON.stringify(message));

const macrotask = () =>
  new Promise<void>((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = () => resolve();
    channel.port2.postMessage(0);
  });
const nextFrame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
const HEX = "a".repeat(64);
const SAMPLES = 15;
/** The large shapes take long enough that a few samples say it. */
const samplesOf = (shape: string) => (shape.startsWith("value-") || shape === "lines-long" ? 5 : SAMPLES);

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "manual",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
};

/** One item value at the 2 MiB bound in a first question, against a short one. */
const VALUES: Record<string, () => string> = {
  "value-small": () => "touch /tmp/marker",
  "value-ascii": () => "word ".repeat(400_000),
  "value-hidden": () => "\u200b".repeat(690_000),
  "value-alternating": () => "a\u200b".repeat(500_000),
};

/** `rows` drawn rows of an instruction file. `wide` lines wrap to several screen lines each, near what 2 MiB allows;
 *  `escapes` puts all of 256 KiB of hidden characters on them; `lines-long` makes them seven of 256 KiB each. */
function prompt(rows: number, nonce: number, shape: string) {
  if (VALUES[shape] !== undefined) {
    return {
      kind: "trust_prompt", tab: 1, nonce, agent: "Claude", root: "/project", top: "/project",
      fingerprint: HEX, findingsDigest: HEX, state: "untrusted", remember: "yes", rememberNote: null, changed: null,
      items: [{ what: "hook", file: ".claude/settings.json", label: "SessionStart", value: VALUES[shape](), outside: false }],
    };
  }
  const filler = shape === "wide" ? " and the rest of a long line that wraps".repeat(20) : "";
  const hidden = shape === "escapes" ? "\u0001".repeat(Math.floor(262_144 / rows)) : "";
  const long = shape === "lines-long" ? "long line, ".repeat(23_800) : "";
  const lines = Array.from({ length: shape === "lines-long" ? 7 : rows }, (_, i) => ({ kind: "added", text: `rule ${i}: keep this as it is${filler}${hidden}${long}` }));
  return {
    kind: "trust_prompt", tab: 1, nonce, agent: "Claude", root: "/project", top: "/project",
    fingerprint: HEX, findingsDigest: HEX, state: "changed", remember: "yes", rememberNote: null,
    changed: {
      added: [], removed: [], changed: ["CLAUDE.md"],
      texts: [{ kind: "diff", path: "CLAUDE.md", added: rows, removed: 0, skippedAfter: 0, hunks: [{ skipped: 0, lines }] }],
    },
    items: [{ what: "memory", file: "CLAUDE.md", label: "memory file", value: "present", outside: false }],
  };
}

async function until(holds: () => boolean) {
  for (let i = 0; i < 20_000 && !holds(); i++) await macrotask();
  if (!holds()) throw new Error("the page did not get there");
}

async function run(rows: number, shape: string) {
  for (let i = 0; i < 400 && window.__eitriDispatch === undefined; i++) await new Promise((resolve) => setTimeout(resolve, 25));
  const dispatch = (payload: unknown) => window.__eitriDispatch!(JSON.stringify(payload));
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  await nextFrame();
  await nextFrame();
  const open = () => document.querySelector(".trust-prompt") !== null;
  const samples: number[] = [];
  let drawn = 0;
  let elements = 0;
  for (let nonce = 1; nonce <= samplesOf(shape); nonce++) {
    const message = prompt(rows, nonce, shape);
    const t0 = performance.now();
    dispatch(message);
    await until(open);
    await nextFrame();
    await nextFrame();
    samples.push(performance.now() - t0);
    drawn = document.querySelectorAll(".trust-text-diff .diff-line").length;
    elements = document.querySelectorAll(".trust-prompt *").length;
    document.querySelector(".trust-prompt")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await until(() => !open());
    await nextFrame();
    await nextFrame();
  }
  const sorted = [...samples].sort((a, b) => a - b);
  return { trustRows: rows, shape, drawn, elements, medianMs: sorted[sorted.length >> 1], maxMs: sorted[sorted.length - 1], samplesMs: samples };
}

(window as unknown as { __trustPaintBench: (rows: number, shape: string) => void }).__trustPaintBench = (rows, shape) => {
  run(rows, shape).then(
    (result) => post({ result }),
    (error: unknown) => post({ error: String(error) }),
  );
};

export {};
