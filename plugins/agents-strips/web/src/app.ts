import type { Bridge, Host, Kit, KeyRow, KeysKit } from "./agents";
import { Slots } from "./slots";
import { render, renderLine } from "./view";

export const OFF_LINE = "agents are off in this window";
/** The plugin store key of the marked rows, and how many it keeps (the oldest mark goes first). */
export const MARKS = "marks";
export const MAX_MARKS = 64;
const MARK_ROW: KeyRow = { action: "strips.mark", keys: ["m"], context: "Agents", desc: "Mark agent" };

/** How often a page on screen reads the marks again: the store sends no event, and another window's sidebar may have
 *  changed them. */
export const MARKS_EVERY_MS = 2_000;

export type BootOptions = { now?: () => number; tick?: boolean; marksEvery?: number };
export type Page = { stop(): void };

function codeOf(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && typeof (error as { code?: unknown }).code === "string"
    ? (error as { code: string }).code : undefined;
}
const text = (error: unknown) => error instanceof Error ? error.message : String(error);

/** The kit declares the shared keys and replaces any earlier installation, so the sidebar's own `m` rides along in
 *  the kit's one installation: the keys kit the agents kit sees adds the row and runs it here. */
async function bindWithMark(win: Host, kit: Kit, opts: Parameters<Kit["bindKeys"]>[0], mark: () => void) {
  const keys = win.eitriKeys;
  if (!keys) return kit.bindKeys(opts);
  const own: KeysKit = {
    ...keys,
    install: o => keys.install({
      ...o,
      rows: [...o.rows, MARK_ROW],
      run: (action, ev) => {
        if (action !== MARK_ROW.action) return o.run(action, ev);
        mark();
        return true;
      },
    }),
  };
  win.eitriKeys = own;
  try {
    return await kit.bindKeys(opts);
  } finally {
    if (win.eitriKeys === own) win.eitriKeys = keys;
  }
}

export async function boot(doc: Document, win: Host, opts: BootOptions = {}): Promise<Page> {
  const now = opts.now ?? Date.now;
  const root = doc.getElementById("strips") ?? doc.body;
  const cleanups: Array<() => void> = [];
  const page: Page = { stop: () => cleanups.splice(0).reverse().forEach(fn => fn()) };
  const kit = win.eitriAgents;
  const bridge: Bridge | undefined = win.eitriPlugin;
  if (!kit || !bridge) {
    renderLine(root, "not connected to Eitri");
    return page;
  }
  let settings: Record<string, unknown> = {};
  try {
    const hello = await bridge.call("host.hello", {}) as { settings?: Record<string, unknown> } | null;
    settings = hello?.settings ?? {};
  } catch {
    // The defaults below stand.
  }
  const narrow = settings.narrow === true;
  const showIdle = settings.show_idle !== false;
  try {
    await kit.start();
  } catch (error) {
    renderLine(root, codeOf(error) === "disabled" ? OFF_LINE : text(error));
    return page;
  }

  const slots = new Slots();
  const marks = new Set<string>();
  let filter = "";
  let cursor: string | null = null;
  // When the list would next read differently with nothing else happening: the earliest age it shows moving on, or a
  // held place turning empty. `null`: only an event can change it.
  let next: number | null = null;
  // Starts and stops the timers after each draw; installed below, once the marks can be read.
  let schedule = () => {};
  const draw = () => {
    const at = now();
    slots.update(kit.store.rows(), at);
    const drawn = render(root, { groups: slots.groups(), kit, marks, filter, now: at, narrow, showIdle });
    const expiry = slots.nextExpiry();
    next = drawn.next === null ? expiry : expiry === null ? drawn.next : Math.min(drawn.next, expiry);
    const selected = kit.store.selection();
    if (selected !== cursor) {
      cursor = selected;
      const node = [...root.querySelectorAll<HTMLElement>(".row[data-key]")].find(n => n.dataset.key === selected);
      node?.scrollIntoView?.({ block: "nearest" });
    }
    schedule();
  };
  cleanups.push(kit.store.on(draw));
  draw();

  // The oldest marks go first, so a list keeps its order and drops from the front.
  const keep = (list: Set<string>) => {
    for (const old of list) {
      if (list.size <= MAX_MARKS) break;
      list.delete(old);
    }
  };
  const stored = async (): Promise<Set<string>> => {
    const saved = await bridge.call("state.read", { key: MARKS }) as { value?: unknown } | null;
    return new Set(Array.isArray(saved?.value) ? saved.value.filter((k): k is string => typeof k === "string") : []);
  };
  // What the page shows is the list last known to be stored (`confirmed`: read, or written by this page) with the `m`s
  // still waiting to be written applied over it, in order. A write that fails drops its own `m` and nothing else.
  let confirmed = new Set<string>();
  const pending: Array<{ key: string; add: boolean }> = [];
  const show = () => {
    const next = new Set(confirmed);
    for (const op of pending) {
      next.delete(op.key);
      if (op.add) next.add(op.key);
      keep(next);
    }
    marks.clear();
    for (const k of next) marks.add(k);
    draw();
  };
  // Every `m` made here bumps `made`; a read that started before one, or while one waits to be written, is older than
  // what the page shows and is dropped.
  let made = 0;
  let reading = false;
  const refresh = async () => {
    if (reading || pending.length > 0) return;
    reading = true;
    const at = made;
    try {
      const next = await stored();
      if (at !== made || pending.length > 0) return;
      confirmed = next;
      if (next.size === marks.size && [...next].every(k => marks.has(k))) return;
      show();
    } catch {
      // No marks yet, or the store is unavailable: the sidebar works without them.
    } finally {
      reading = false;
    }
  };
  await refresh();

  const say = (message: string) => win.eitriKeys?.message(message);
  const order = () => slots.groups().flatMap(group =>
    group.slots.flatMap(slot => slot.key !== null && kit.store.byKey(slot.key) ? [slot.key] : []));
  // The store is the plugin's, shared by every window's sidebar, so each `m` is applied to the list as stored when it
  // is written, not to the list this page read at start: another window's marks stay. Which way an `m` goes is what
  // this page shows. Marks made here write one after another, each reading what the one before wrote.
  let writes: Promise<void> = Promise.resolve();
  const mark = () => {
    const key = kit.store.selection();
    if (key === null || !order().includes(key) || !kit.store.byKey(key)) return;
    const op = { key, add: !marks.has(key) };
    pending.push(op);
    made += 1;
    show();
    writes = writes.then(async () => {
      let next: Set<string>;
      try {
        next = await stored();
      } catch {
        // Unreadable now: what this page last knew to be stored is the best there is.
        next = new Set(confirmed);
      }
      next.delete(op.key);
      if (op.add) next.add(op.key);
      keep(next);
      try {
        await bridge.call("state.write", { key: MARKS, value: [...next] });
        confirmed = next;
      } catch (error) {
        // Nothing was stored: this `m` is taken back, the ones still waiting stay.
        say(text(error));
      }
    }).catch(error => console.error(error)).finally(() => {
      pending.splice(pending.indexOf(op), 1);
      show();
    });
  };
  try {
    cleanups.push(await bindWithMark(win, kit, { order, onFilter: q => { filter = q; draw(); } }, mark));
  } catch (error) {
    console.error(error);
  }

  const keyAt = (target: EventTarget | null) =>
    target instanceof Element ? target.closest<HTMLElement>(".row[data-key]")?.dataset.key : undefined;
  const ask = (verb: string, args: unknown) => { bridge.call(verb, args).catch(error => say(text(error))); };
  const select = (key: string) => ask("agents.select", { key });
  const open = (key: string) => {
    const row = kit.store.byKey(key);
    if (row?.class === "needs") ask("agents.round", { op: "start", key: row.key });
    else if (row) ask("agents.focus", { id: row.id });
  };
  // The list is drawn afresh whenever what it shows changes (each second while it shows an age in seconds), and a
  // click (or double click) whose pressed node left the document before the release reaches no row: an engine sends none, or sends it to an ancestor. So a click is
  // a press and a release on the same row, matched by the row's key rather than its node: the release selects, and
  // the release of a double click (its click count) opens, as `Enter` does.
  // A press whose release the list never sees (released outside the sidebar or the page, or a cancelled pointer) is
  // forgotten, so a later release with no press of its own behind it selects nothing.
  let pressed: string | undefined;
  const press = (ev: MouseEvent) => { pressed = ev.button === 0 ? keyAt(ev.target) : undefined; };
  const forget = () => { pressed = undefined; };
  const outside = (ev: Event) => { if (!(ev.target instanceof Node && root.contains(ev.target))) forget(); };
  const left = (ev: MouseEvent) => { if (ev.buttons === 0) forget(); };
  const release = (ev: MouseEvent) => {
    const key = pressed;
    pressed = undefined;
    if (ev.button !== 0 || key === undefined || keyAt(ev.target) !== key) return;
    select(key);
    if (ev.detail === 2) open(key);
  };
  // A click with no press behind it (count 0: assistive technology, a script) selects too.
  const click = (ev: MouseEvent) => {
    const key = ev.detail === 0 ? keyAt(ev.target) : undefined;
    if (key) select(key);
  };
  root.addEventListener("mousedown", press);
  root.addEventListener("mouseup", release);
  root.addEventListener("click", click);
  root.addEventListener("mouseleave", left);
  doc.addEventListener("mouseup", outside);
  doc.addEventListener("pointercancel", forget);
  win.addEventListener("blur", forget);
  cleanups.push(
    () => root.removeEventListener("mousedown", press),
    () => root.removeEventListener("mouseup", release),
    () => root.removeEventListener("click", click),
    () => root.removeEventListener("mouseleave", left),
    () => doc.removeEventListener("mouseup", outside),
    () => doc.removeEventListener("pointercancel", forget),
    () => win.removeEventListener("blur", forget),
  );

  // The plugin store sends no event when another window's sidebar writes the marks, so a page on screen reads them
  // again now and then while it shows rows to mark, and at once when it comes back on screen. Every timer here runs
  // only while the page is on screen and has something it would change: with no agents nothing runs.
  const every = opts.marksEvery ?? (opts.tick === false ? 0 : MARKS_EVERY_MS);
  let marksTimer: number | undefined;
  let marksRuns = 0;
  let clockTimer: number | undefined;
  let clockAt: number | null = null;
  const stopClock = () => {
    if (clockTimer !== undefined) win.clearTimeout(clockTimer);
    clockTimer = undefined;
    clockAt = null;
  };
  const stopMarks = () => {
    if (marksTimer !== undefined) win.clearInterval(marksTimer);
    marksTimer = undefined;
  };
  schedule = () => {
    const visible = !doc.hidden;
    const wantMarks = every > 0 && visible && slots.groups().length > 0;
    if (wantMarks && marksTimer === undefined) {
      marksTimer = win.setInterval(() => void refresh(), every);
      // Rows came back after a stretch with none: another window may have marked meanwhile.
      if (marksRuns++ > 0) void refresh();
    } else if (!wantMarks) {
      stopMarks();
    }
    const want = opts.tick !== false && visible ? next : null;
    if (want === clockAt && (want === null || clockTimer !== undefined)) return;
    stopClock();
    if (want === null) return;
    clockAt = want;
    clockTimer = win.setTimeout(() => {
      clockTimer = undefined;
      clockAt = null;
      draw();
    }, Math.max(0, want - now()));
  };
  const onVisibility = () => {
    if (doc.hidden) {
      schedule();
      return;
    }
    // Back on screen: the ages moved while it was away.
    draw();
    void refresh();
  };
  doc.addEventListener("visibilitychange", onVisibility);
  cleanups.push(() => {
    doc.removeEventListener("visibilitychange", onVisibility);
    schedule = () => {};
    stopClock();
    stopMarks();
  });
  schedule();
  return page;
}
