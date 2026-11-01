import { renderMarkdown, forgetMarkdown } from "../markdown";
import { buildDisplay } from "../display";
import { applyEvents } from "../reducer";
import { buildTimeline } from "../timeline";
import type { AgentDomainEvent, AgentUiState } from "../types";
import { longSessionState, longSessionTurnTimes } from "./longSession";

/* Time long-session operations in a running panel under jsdom/vitest or real WebKit.
   The separately bundled `longSessionBenchEntry.ts` drives the page through `__eitriDispatch`
   and DOM events on its elements, so measurements exercise the shipped code. */

/** What separates the two pages: how to wait until one delivery has been worked through. */
export type BenchHost = {
  /** Runs `act`, and resolves when the page has finished what it started: the time the page took. */
  work: (act: () => void) => Promise<number>;
  /** Resolves when the page is ready for the next sample, as the pump would wait between two batches. */
  pause: () => Promise<void>;
};

export type Stat = { n: number; median: number; p90: number; total: number };
export type BenchResult = {
  rows: number;
  snapshotBytes: number;
  drawnRows: number;
  operations: Record<string, Stat>;
  /** How long the two frames after each sample took: above two frame times, the page dropped frames. */
  frames: Record<string, Stat>;
  /** Time inside forced layout reads and `innerHTML` writes while the operation ran, sums over its samples. */
  buckets: Record<string, { layoutMs: number; htmlMs: number; samples: number }>;
  /** The pure parts on their own, each timed over the same conversation in this page's engine. */
  parts: { foldOneDeltaMs: number; timelineMs: number; markdownOfTailMs: number };
};

const TAB = { number: 1, name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null };

function stat(samples: number[]): Stat {
  const sorted = [...samples].sort((a, b) => a - b);
  const at = (q: number) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] ?? 0;
  return { n: samples.length, median: at(0.5), p90: at(0.9), total: samples.reduce((a, b) => a + b, 0) };
}

/** Times what the page does while `run` runs inside forced layout and `innerHTML`, by wrapping the DOM's own
 *  accessors: `run` is measured, nothing about what it does changes. */
function timeDomReads() {
  let layout = 0;
  let html = 0;
  const wrapGetter = (proto: object, name: string) => {
    const was = Object.getOwnPropertyDescriptor(proto, name);
    if (was?.get === undefined) return;
    const get = was.get;
    Object.defineProperty(proto, name, {
      ...was,
      get(this: unknown) {
        const t0 = performance.now();
        const value = get.call(this);
        layout += performance.now() - t0;
        return value;
      },
    });
  };
  for (const name of ["scrollHeight", "clientHeight", "clientWidth"]) wrapGetter(Element.prototype, name);
  for (const name of ["offsetTop", "offsetLeft", "offsetWidth", "offsetHeight"]) wrapGetter(HTMLElement.prototype, name);
  const inner = Object.getOwnPropertyDescriptor(Element.prototype, "innerHTML");
  if (inner?.set !== undefined) {
    const set = inner.set;
    Object.defineProperty(Element.prototype, "innerHTML", {
      ...inner,
      set(this: Element, value: string) {
        const t0 = performance.now();
        set.call(this, value);
        html += performance.now() - t0;
      },
    });
  }
  return {
    take() {
      const taken = { layoutMs: layout, htmlMs: html };
      layout = 0;
      html = 0;
      return taken;
    },
  };
}

function typeInto(box: HTMLTextAreaElement, key: string) {
  box.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!;
  setter.call(box, box.value + key);
  box.dispatchEvent(new Event("input", { bubbles: true }));
  box.dispatchEvent(new KeyboardEvent("keyup", { key, bubbles: true, cancelable: true }));
}

function press(target: Element, key: string) {
  target.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  target.dispatchEvent(new KeyboardEvent("keyup", { key, bubbles: true, cancelable: true }));
}

const DELTA_LINES = [
  "The next part of the answer goes through the writer once more, because `flush` runs after the loop and not",
  "in it, which is why only the first row ever reached the file. Here is the smallest change that keeps that:\n\n",
  "```rust\nfn flush_each(rows: &[Row], out: &mut Writer) -> io::Result<()> {\n    for row in rows {\n",
  "        out.write_row(row)?;\n        out.flush()?;\n    }\n    Ok(())\n}\n```\n\n",
  "- the loop stays where it was\n- every row is on disk before the next one is read\n\n",
];

export type ReplyShape = "blocks" | "paragraph" | "fence";

/** The `i`th batch of a long reply, about `size` characters. `blocks`: what a long answer looks like, paragraphs, fences
 *  and lists. `paragraph`: one line after another with no blank line, so the whole reply is one open block. `fence`: a
 *  file printed into one code block that does not end. */
export function replyBatch(shape: ReplyShape, i: number, size: number): string {
  const head = `line ${i} `;
  const filler = "x".repeat(Math.max(0, size - head.length - 1));
  if (shape === "paragraph") return `${head}${filler}\n`;
  if (shape === "fence") return `${i === 0 ? "```rust\n" : ""}let value_${i} = "${filler}";\n`;
  const text = DELTA_LINES.join("");
  let out = "";
  while (out.length < size) out += text;
  return out;
}

export type ReplyResult = { bytes: number; shape: ReplyShape; batches: number; totalMs: number; firstTenthMs: number; lastTenthMs: number; endMs: number };

/** One long reply on a short conversation, batch after batch with no pause, as `longReply.measure.test.tsx` does. */
export async function runLongReplyBench(bytes: number, shape: ReplyShape, host: BenchHost, batchBytes = 1024): Promise<ReplyResult> {
  const dispatch = (payload: unknown) => window.__eitriDispatch!(JSON.stringify(payload));
  const state = longSessionState(44);
  let revision = state.nextSeq;
  await host.work(() => {
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, id: 1, label: "1 long" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: revision, state });
  });
  const deliver = (events: unknown[]) => {
    const payload = { kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + events.length, events };
    revision += events.length;
    dispatch(payload);
  };
  await host.work(() => deliver([{ type: "turn_started", turn_id: "live" }]));
  const batches = Math.ceil(bytes / batchBytes);
  const times: number[] = [];
  for (let i = 0; i < batches; i++) {
    times.push(await host.work(() => deliver([{ type: "content_delta", kind: "text", turn_id: "live", text: replyBatch(shape, i, batchBytes) }])));
  }
  const endMs = await host.work(() =>
    deliver([{ type: "turn_completed", turn_id: "live", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } }]),
  );
  const mean = (xs: number[]) => xs.reduce((a, b) => a + b, 0) / Math.max(1, xs.length);
  const tenth = Math.max(1, Math.floor(batches / 10));
  return { bytes, shape, batches, totalMs: times.reduce((a, b) => a + b, 0), firstTenthMs: mean(times.slice(0, tenth)), lastTenthMs: mean(times.slice(-tenth)), endMs };
}

export async function runLongSessionBench(
  rows: number,
  host: BenchHost,
  samples = 60,
  stage: (name: string) => Promise<void> = async () => {},
): Promise<BenchResult> {
  const dispatch = (payload: unknown) => window.__eitriDispatch!(JSON.stringify(payload));
  const state = longSessionState(rows);
  const snapshotJson = JSON.stringify({ kind: "snapshot", tab: 1, throughRevision: state.nextSeq, state, turnTimes: longSessionTurnTimes(state) });
  const emptyTab = JSON.stringify({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...longSessionState(0), status: { kind: "running" } } });
  const tabs = (active: number) => ({
    kind: "tabs", active,
    tabs: [{ ...TAB, id: 1, label: "1 long" }, { ...TAB, id: 2, number: 2, label: "2 new" }],
  });

  const dom = timeDomReads();
  const operations: Record<string, number[]> = {};
  const frames: Record<string, number[]> = {};
  const buckets: BenchResult["buckets"] = {};
  const timed = async (name: string, act: () => void) => {
    dom.take();
    const ms = await host.work(act);
    const { layoutMs, htmlMs } = dom.take();
    (operations[name] ??= []).push(ms);
    const bucket = (buckets[name] ??= { layoutMs: 0, htmlMs: 0, samples: 0 });
    bucket.layoutMs += layoutMs;
    bucket.htmlMs += htmlMs;
    bucket.samples += 1;
    const t0 = performance.now();
    await host.pause();
    (frames[name] ??= []).push(performance.now() - t0);
  };

  await host.work(() => {
    dispatch({ kind: "hello", backend: "sidecar", projectDir: "/work/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
    dispatch(tabs(1));
  });
  await stage("empty");
  await timed("reload", () => window.__eitriDispatch!(snapshotJson));
  await stage("loaded");
  const drawnRows = document.querySelectorAll(".message-list [data-nav-stop=\"row\"]").length;
  if (document.querySelectorAll(".turn-time").length === 0) throw new Error("no turn's time is drawn");

  // Each size's tab switches: away (an empty tab) is not timed, the way back is.
  for (let i = 0; i < 3; i++) {
    await host.work(() => {
      dispatch(tabs(2));
      window.__eitriDispatch!(emptyTab);
    });
    await host.pause();
    await timed("tab switch back", () => {
      dispatch(tabs(1));
      window.__eitriDispatch!(snapshotJson);
    });
  }

  // Keys in the input box: `i` puts them there, as it does for a person.
  const root = document.querySelector(".agent-ui-conversation")!;
  await host.work(() => press(root, "i"));
  await host.pause();
  const box = document.querySelector<HTMLTextAreaElement>("textarea");
  if (box !== null) {
    for (let i = 0; i < samples; i++) await timed("key in the input box", () => typeInto(box, "abcdefgh"[i % 8]));
    await host.work(() => {
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!;
      setter.call(box, "");
      box.dispatchEvent(new Event("input", { bubbles: true }));
    });
  }

  // One streamed reply, in the pump's batches, at the end of the conversation.
  let revision = state.nextSeq;
  const deliver = (events: unknown[]) => {
    const payload = { kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + events.length, events };
    revision += events.length;
    dispatch(payload);
  };
  await host.work(() => deliver([{ type: "turn_started", turn_id: "live" }]));
  for (let i = 0; i < samples; i++) {
    await timed("streamed delta", () => deliver([{ type: "content_delta", kind: "text", turn_id: "live", text: DELTA_LINES[i % DELTA_LINES.length] }]));
  }
  await timed("turn end", () =>
    deliver([{ type: "turn_completed", turn_id: "live", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } }]),
  );

  // A tool row opening: BROWSE, up from the end to the nearest tool row, Enter opens it, Enter closes it.
  const keysAt = () => document.activeElement ?? root;
  await host.work(() => press(keysAt(), "Escape"));
  await host.pause();
  const onToolRow = () => document.querySelector(".row-current.row-tool") !== null;
  for (let step = 0; step < 60 && !onToolRow(); step++) {
    await host.work(() => press(keysAt(), "k"));
    await host.pause();
  }
  if (!onToolRow()) throw new Error("no tool row reached from the end of the conversation");
  const opened = () => document.querySelectorAll(".row-current .tool-result-body").length;
  const shut = opened();
  for (let i = 0; i < 6; i++) await timed("tool row opening", () => press(keysAt(), "Enter"));
  await host.work(() => press(keysAt(), "Enter"));
  if (opened() === shut) throw new Error("Enter did not open the tool row");

  await stage("used");
  const parts = pureParts(state);
  return {
    rows,
    snapshotBytes: snapshotJson.length,
    drawnRows,
    operations: Object.fromEntries(Object.entries(operations).map(([name, times]) => [name, stat(times)])),
    frames: Object.fromEntries(Object.entries(frames).map(([name, times]) => [name, stat(times)])),
    buckets,
    parts,
  };
}

/** The store, the timeline and one markdown pass of a streaming tail, without the page around them. */
function pureParts(state: AgentUiState) {
  const repeat = (count: number, run: (i: number) => void) => {
    const t0 = performance.now();
    for (let i = 0; i < count; i++) run(i);
    return (performance.now() - t0) / count;
  };
  const open: AgentUiState = { ...state, activeTurnId: "live", assistantMessageOpen: true };
  const delta = { type: "content_delta", kind: "text", turn_id: "live", text: "one more line\n" } as unknown as AgentDomainEvent;
  const foldOneDeltaMs = repeat(200, () => void applyEvents(open, [delta]));
  const timelineMs = repeat(20, () => void buildDisplay(buildTimeline(open), { expanded: {}, detailed: false, turnRunning: true, thinking: "collapsed" }));
  const tail = DELTA_LINES.join("").repeat(40);
  const markdownOfTailMs = repeat(20, (i) => {
    const text = `${tail}${i}`;
    renderMarkdown(text);
    forgetMarkdown(text);
  });
  return { foldOneDeltaMs, timelineMs, markdownOfTailMs };
}
