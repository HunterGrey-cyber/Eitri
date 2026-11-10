import { afterEach, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

type Listener = (data: unknown) => void;
type Installed = {
  rows: Array<{ action: string; keys: string[] }>;
  run(action: string, ev: KeyboardEvent): boolean;
};

const NOW = 1_793_865_600_000;
const PROJECT = { key: "9b2c4e7a10f3d856", name: "eitri" };
const WINDOW = { instance: "a3f09c1d2e4b5f60", name: "1", focused: true };
let ids = 0;

function row(key: string, patch: Record<string, unknown> = {}) {
  ids += 1;
  return {
    id: `${WINDOW.instance}.${ids}`, key, agent: "claude", project: PROJECT, window: WINDOW, number: ids,
    label: key, label_from: "title", shown: false, state: "idle", class: "quiet", since: NOW - 60_000,
    needs: { cards: 0, questions: 0, trust: false }, waiting_since: null, unread: false, tool: null,
    background: { calls: 0, shells: 0, monitors: 0, agents: 0, other: 0 }, queued: 0, mode: "auto",
    ended_how: null, moving: false, ...patch,
  };
}
const card = (key: string, cards = 1) => ({
  state: "card", class: "needs", tool: "Bash", needs: { cards, questions: 0, trust: false }, waiting_since: NOW - 3_000,
  key,
});
function view(rows: unknown[]) {
  return { v: 1, at: NOW, enabled: true, projects: [PROJECT], rows, selection: null, round: null };
}

let stop: (() => void) | undefined;
afterEach(() => {
  stop?.();
  stop = undefined;
  document.body.replaceChildren();
});

type OpenOpts = {
  enabled?: boolean;
  settings?: Record<string, unknown>;
  store?: { value: unknown };
  /** A `state.write` that fails, with this error. */
  writeFails?: () => Error | undefined;
  marksEvery?: number;
  tick?: boolean;
  now?: () => number;
  lookupRow?: ReturnType<typeof row>;
};

async function open(rows: unknown[], opts: OpenOpts = {}) {
  // The plugin store, shared by every window's sidebar: a test may write it as another window would.
  const store = opts.store ?? { value: null };
  vi.resetModules();
  document.body.innerHTML = '<main id="strips"></main>';
  const listeners = new Map<string, Set<Listener>>();
  const call = vi.fn(async (verb: string, _args?: unknown) => {
    if (verb === "host.hello") {
      return {
        capabilities: ["agents", "state"], timeoutlen: 1000, prefix: "Ctrl+b", keys_settings: {},
        settings: opts.settings ?? { narrow: false, show_idle: true },
      };
    }
    if (verb.startsWith("agents.") && opts.enabled === false) {
      throw Object.assign(new Error("the agents layer is off in this window"), { code: "disabled" });
    }
    if (verb === "agents.watch") return view(rows);
    if (verb === "state.read") return { value: store.value };
    if (verb === "state.write") {
      const failure = opts.writeFails?.();
      if (failure) throw failure;
      store.value = (_args as { value: unknown }).value;
      return {};
    }
    return {};
  });
  let installed: Installed | undefined;
  Object.defineProperty(window, "eitriPlugin", {
    configurable: true, writable: true, value: {
      call, on(event: string, fn: Listener) {
        if (!listeners.has(event)) listeners.set(event, new Set());
        listeners.get(event)!.add(fn);
        return () => { listeners.get(event)?.delete(fn); };
      },
    },
  });
  Object.defineProperty(window, "eitriKeys", {
    configurable: true, writable: true, value: {
      install: vi.fn(async (o: Installed) => { installed = o; return () => {}; }), message: vi.fn(),
    },
  });
  await import("../../../../plugin-kit/agents/src/index");
  if (opts.lookupRow) {
    const kit = window.eitriAgents!;
    window.eitriAgents = { ...kit, store: { ...kit.store,
      byKey: key => key === opts.lookupRow!.key ? opts.lookupRow! : kit.store.byKey(key),
    } };
  }
  const { boot } = await import("./app");
  const page = await boot(document, window, {
    now: opts.now ?? (() => NOW), tick: opts.tick ?? false, marksEvery: opts.marksEvery,
  });
  stop = page.stop;
  const root = document.getElementById("strips")!;
  const emit = (event: string, data: unknown) => listeners.get(event)?.forEach(fn => fn(data));
  const slots = () => [...root.querySelectorAll<HTMLElement>(".row")].map(el => el.dataset.key ?? "");
  const at = (key: string) => root.querySelector<HTMLElement>(`.row[data-key="${key}"]`)!;
  return { root, call, emit, slots, at, keys: () => installed! };
}

it("a_row_keeps_its_slot_through_state_changes", async () => {
  const page = await open([row("k-a"), row("k-b"), row("k-c")]);
  expect(page.slots()).toEqual(["k-a", "k-b", "k-c"]);
  page.emit("agents.row", row("k-c", { state: "running", class: "working", tool: "Edit", number: 3 }));
  page.emit("agents.row", row("k-a", { state: "done", class: "quiet", unread: true, number: 9 }));
  expect(page.slots()).toEqual(["k-a", "k-b", "k-c"]);
  expect(page.at("k-c").querySelector(".glyph")!.textContent).toBe("●");
  expect(page.at("k-a").querySelector(".glyph")!.textContent).toBe("•");
});

it("a_moved_row_keeps_its_slot", async () => {
  const a = row("k-a");
  const b = row("k-b");
  const page = await open([a, b, row("k-c")]);
  page.emit("agents.gone", { id: b.id, key: "k-b", why: "moved" });
  expect(page.slots()).toEqual(["k-a", "", "k-c"]);
  expect(page.root.querySelectorAll(".row.empty")).toHaveLength(1);
  page.emit("agents.row", row("k-b", { window: { instance: "77aa00bb11cc22dd", name: "2", focused: false } }));
  expect(page.slots()).toEqual(["k-a", "k-b", "k-c"]);
});

it("needs_rows_are_pushed_out_nothing_resorts", async () => {
  const page = await open([row("k-a"), row("k-b"), row("k-c")]);
  page.emit("agents.row", row("k-c", card("k-c")));
  expect(page.slots()).toEqual(["k-a", "k-b", "k-c"]);
  expect(page.at("k-c").classList.contains("out")).toBe(true);
  expect(page.at("k-c").querySelector(".edge")!.textContent).toBe("◀");
  expect(page.at("k-a").classList.contains("out")).toBe(false);
  expect(page.at("k-a").querySelector(".edge")!.textContent).toBe("");
});

it("the_narrow_rail_shows_glyph_and_count", async () => {
  const page = await open([
    row("k-a", card("k-a")),
    row("k-b", { state: "running", class: "working", tool: "Edit" }),
    row("k-c", { state: "question", class: "needs", needs: { cards: 0, questions: 2, trust: false } }),
  ], { settings: { narrow: true, show_idle: true } });
  expect(page.root.querySelector(".narrow")).not.toBeNull();
  const text = (key: string) => page.at(key).textContent;
  expect(text("k-a")).toBe("⚑1◀");
  expect(text("k-b")).toBe("●");
  expect(text("k-c")).toBe("?2◀");
  expect(page.at("k-a").querySelector(".label")).toBeNull();
  expect(page.at("k-a").title).toContain("k-a");
});

it("the_round_current_stop_is_marked", async () => {
  const page = await open([row("k-a", card("k-a")), row("k-b", card("k-b")), row("k-c")]);
  page.emit("agents.round", {
    current: "k-b", visited: ["k-a"], started_at: NOW - 5_000, at: NOW,
    from: { instance: WINDOW.instance, module: "agent", tab: 1 },
  });
  expect(page.at("k-b").classList.contains("current")).toBe(true);
  expect(page.at("k-b").querySelector(".stop")!.textContent).toBe("▸");
  expect(page.at("k-a").classList.contains("current")).toBe(false);
  expect(page.at("k-a").querySelector(".stop")!.textContent).toBe("");
  page.emit("agents.round", null);
  expect(page.root.querySelector(".row.current")).toBeNull();
});

it("the_shared_selection_moves_the_cursor", async () => {
  const page = await open([row("k-a"), row("k-b"), row("k-c")]);
  expect(page.root.querySelector(".row.cursor")).toBeNull();
  page.emit("agents.selection", { key: "k-a", at: NOW, by: "77aa00bb11cc22dd" });
  expect(page.at("k-a").classList.contains("cursor")).toBe(true);
  page.emit("agents.selection", { key: "k-c", at: NOW, by: WINDOW.instance });
  expect(page.at("k-c").classList.contains("cursor")).toBe(true);
  expect(page.at("k-a").classList.contains("cursor")).toBe(false);
  page.keys().run("agents.next", new KeyboardEvent("keydown"));
  expect(page.call).toHaveBeenLastCalledWith("agents.select", { key: "k-c" });
  page.keys().run("agents.prev", new KeyboardEvent("keydown"));
  expect(page.call).toHaveBeenLastCalledWith("agents.select", { key: "k-b" });
});

it("slash_dims_and_never_moves", async () => {
  const page = await open([
    row("k-a", { label: "close question" }), row("k-b", { label: "tray chips" }), row("k-c", { label: "keys guide" }),
  ]);
  page.keys().run("agents.filter", new KeyboardEvent("keydown"));
  const prompt = document.querySelector<HTMLInputElement>('input[type="search"]')!;
  prompt.value = "chip";
  prompt.dispatchEvent(new Event("input"));
  expect(page.slots()).toEqual(["k-a", "k-b", "k-c"]);
  expect(page.at("k-a").classList.contains("dim")).toBe(true);
  expect(page.at("k-b").classList.contains("dim")).toBe(false);
  expect(page.at("k-c").classList.contains("dim")).toBe(true);
  prompt.value = "";
  prompt.dispatchEvent(new Event("input"));
  expect(page.root.querySelectorAll(".row.dim")).toHaveLength(0);
});

it("m_marks_into_state", async () => {
  const page = await open([row("k-a"), row("k-b")]);
  const mark = page.keys().rows.find(r => r.keys.includes("m"));
  expect(mark).toBeDefined();
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  expect(page.keys().run(mark!.action, new KeyboardEvent("keydown"))).toBe(true);
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
  expect(page.at("k-a").classList.contains("marked")).toBe(false);
  await vi.waitFor(() => expect(page.call).toHaveBeenLastCalledWith("state.write", { key: "marks", value: ["k-b"] }));
  page.keys().run(mark!.action, new KeyboardEvent("keydown"));
  expect(page.at("k-b").classList.contains("marked")).toBe(false);
  await vi.waitFor(() => expect(page.call).toHaveBeenLastCalledWith("state.write", { key: "marks", value: [] }));
  page.keys().run("agents.next", new KeyboardEvent("keydown"));
  expect(page.call).toHaveBeenLastCalledWith("agents.select", { key: "k-b" });
});

const markRow = (page: Awaited<ReturnType<typeof open>>) => page.keys().rows.find(r => r.keys.includes("m"))!;

it("m_cannot_mark_a_selection_outside_the_displayed_order", async () => {
  const page = await open([row("k-b")], { lookupRow: row("k-a") });
  page.emit("agents.selection", { key: "k-a", at: NOW, by: WINDOW.instance });
  expect(page.at("k-a")).toBeNull();
  page.call.mockClear();
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await Promise.resolve();
  await Promise.resolve();
  expect(page.call).not.toHaveBeenCalled();
});

it("a_mark_keeps_the_marks_another_window_wrote", async () => {
  const store: { value: unknown } = { value: null };
  const page = await open([row("k-a"), row("k-b")], { store });
  // Another window's sidebar marks k-a after this one read the store.
  store.value = ["k-a"];
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual(["k-a", "k-b"]));
  await vi.waitFor(() => expect(page.at("k-a").classList.contains("marked")).toBe(true));
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
  // Unmarking takes only that key out of what is stored.
  store.value = ["k-c", "k-a", "k-b"];
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual(["k-c", "k-a"]));
});

it("a_full_mark_list_drops_its_oldest_stored_mark", async () => {
  const old = Array.from({ length: 64 }, (_, i) => `k-old${i}`);
  const store: { value: unknown } = { value: [] };
  const page = await open([row("k-a"), row("k-b")], { store });
  store.value = old;
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual([...old.slice(1), "k-b"]));
});

it("marks_another_window_writes_show_without_a_write_here", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  try {
    const store: { value: unknown } = { value: null };
    const page = await open([row("k-a"), row("k-b")], { store, marksEvery: 2_000 });
    expect(page.at("k-a").classList.contains("marked")).toBe(false);
    // Another window's sidebar marks k-a; this page writes nothing.
    store.value = ["k-a"];
    vi.advanceTimersByTime(2_000);
    await vi.waitFor(() => expect(page.at("k-a").classList.contains("marked")).toBe(true));
    expect(page.at("k-b").classList.contains("marked")).toBe(false);
    // And when the page comes back on screen, at once.
    store.value = ["k-b"];
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.waitFor(() => expect(page.at("k-b").classList.contains("marked")).toBe(true));
    expect(page.at("k-a").classList.contains("marked")).toBe(false);
  } finally {
    vi.useRealTimers();
  }
});

it("a_read_older_than_a_mark_made_here_is_dropped", async () => {
  const store: { value: unknown } = { value: [] };
  const page = await open([row("k-a"), row("k-b")], { store });
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  const call = page.call.getMockImplementation()!;
  // The refresh's read is held; meanwhile `m` marks k-b here and its write lands.
  page.call.mockImplementationOnce(async (verb: string, args?: unknown) => {
    await held;
    return call(verb, args);
  });
  document.dispatchEvent(new Event("visibilitychange"));
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual(["k-b"]));
  // The held read now answers what it would have read before: the page keeps the mark made here.
  store.value = [];
  release();
  await new Promise(resolve => setTimeout(resolve, 20));
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
});

it("a_failed_mark_write_takes_the_toggle_back", async () => {
  const store: { value: unknown } = { value: [] };
  let fail = true;
  const page = await open([row("k-a"), row("k-b")], {
    store, writeFails: () => fail ? Object.assign(new Error("the plugin's store did not answer"), { code: "io" }) : undefined,
  });
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  // Shown at once, before the store answers.
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
  await vi.waitFor(() => expect(page.at("k-b").classList.contains("marked")).toBe(false));
  expect(store.value).toEqual([]);
  expect((window as unknown as { eitriKeys: { message: ReturnType<typeof vi.fn> } }).eitriKeys.message)
    .toHaveBeenCalledWith("the plugin's store did not answer");
  // An unmark that fails puts the mark back.
  fail = false;
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual(["k-b"]));
  fail = true;
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  expect(page.at("k-b").classList.contains("marked")).toBe(false);
  await vi.waitFor(() => expect(page.at("k-b").classList.contains("marked")).toBe(true));
  expect(store.value).toEqual(["k-b"]);
});

it("two_marks_whose_writes_both_fail_leave_nothing_marked", async () => {
  const store: { value: unknown } = { value: [] };
  const page = await open([row("k-a"), row("k-b")], {
    store, writeFails: () => Object.assign(new Error("the plugin's store did not answer"), { code: "io" }),
  });
  // `m` on A, then `m` on B before A's write has failed: both show at once.
  page.emit("agents.selection", { key: "k-a", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  expect(page.at("k-a").classList.contains("marked")).toBe(true);
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
  // Neither was stored: neither stays marked, and the store is as it was.
  await vi.waitFor(() => expect(page.root.querySelectorAll(".row.marked")).toHaveLength(0));
  expect(store.value).toEqual([]);
});

it("a_failed_write_keeps_a_later_mark_that_was_stored", async () => {
  const store: { value: unknown } = { value: [] };
  let writes = 0;
  const page = await open([row("k-a"), row("k-b")], {
    store, writeFails: () => (writes++ === 0 ? Object.assign(new Error("busy"), { code: "busy" }) : undefined),
  });
  page.emit("agents.selection", { key: "k-a", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  page.emit("agents.selection", { key: "k-b", at: NOW, by: WINDOW.instance });
  page.keys().run(markRow(page).action, new KeyboardEvent("keydown"));
  await vi.waitFor(() => expect(store.value).toEqual(["k-b"]));
  await vi.waitFor(() => expect(page.at("k-a").classList.contains("marked")).toBe(false));
  expect(page.at("k-b").classList.contains("marked")).toBe(true);
});

const mouse = (type: string, el: Element, detail = 1, button = 0) =>
  el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button, detail }));

it("a_click_selects_though_a_redraw_replaces_the_row_mid_click", async () => {
  const page = await open([row("k-a"), row("k-b")]);
  const pressed = page.at("k-b");
  mouse("mousedown", pressed);
  // The clock or an event redraws between press and release: the pressed node leaves the document, and the engine
  // then sends no click at all, only the release on the new row under the pointer.
  page.emit("agents.row", row("k-a", { state: "running", class: "working", tool: "Edit", number: 1 }));
  expect(pressed.isConnected).toBe(false);
  expect(page.call).not.toHaveBeenCalledWith("agents.select", expect.anything());
  mouse("mouseup", page.at("k-b"));
  expect(page.call).toHaveBeenCalledWith("agents.select", { key: "k-b" });
  // With nothing redrawn the click that follows the release selects nothing more.
  page.call.mockClear();
  mouse("mousedown", page.at("k-a"));
  mouse("mouseup", page.at("k-a"));
  mouse("click", page.at("k-a"));
  expect(page.call.mock.calls.filter(c => c[0] === "agents.select")).toEqual([["agents.select", { key: "k-a" }]]);
  // Pressed on one row and released on another is no click; nor is another button.
  page.call.mockClear();
  mouse("mousedown", page.at("k-a"));
  mouse("mouseup", page.at("k-b"));
  mouse("mousedown", page.at("k-a"), 1, 2);
  mouse("mouseup", page.at("k-a"), 1, 2);
  expect(page.call).not.toHaveBeenCalledWith("agents.select", expect.anything());
  // A click with no press behind it (assistive technology) still selects.
  mouse("click", page.at("k-b"), 0);
  expect(page.call).toHaveBeenCalledWith("agents.select", { key: "k-b" });
});

it("a_press_released_outside_the_sidebar_is_forgotten", async () => {
  const page = await open([row("k-a"), row("k-b")]);
  const outside = document.createElement("div");
  document.body.append(outside);
  // Pressed on a row, released outside the sidebar: no click. A later release on that row with no press of its own
  // (a drag that came in from outside) selects nothing either.
  mouse("mousedown", page.at("k-a"));
  mouse("mouseup", outside);
  mouse("mouseup", page.at("k-a"));
  expect(page.call).not.toHaveBeenCalledWith("agents.select", expect.anything());
  // The same after a cancelled pointer, and after the pointer left the sidebar with no button held.
  mouse("mousedown", page.at("k-a"));
  document.dispatchEvent(new Event("pointercancel"));
  mouse("mouseup", page.at("k-a"));
  mouse("mousedown", page.at("k-b"));
  page.root.dispatchEvent(new MouseEvent("mouseleave", { buttons: 0 }));
  mouse("mouseup", page.at("k-b"));
  expect(page.call).not.toHaveBeenCalledWith("agents.select", expect.anything());
  // A drag out and back with the button held keeps the press: its release on the row is a click.
  mouse("mousedown", page.at("k-b"));
  page.root.dispatchEvent(new MouseEvent("mouseleave", { buttons: 1 }));
  mouse("mouseup", page.at("k-b"));
  expect(page.call).toHaveBeenCalledWith("agents.select", { key: "k-b" });
});

it("a_double_click_opens_though_a_redraw_comes_between_its_clicks", async () => {
  const a = row("k-a");
  const b = row("k-b", card("k-b"));
  const page = await open([a, b]);
  const double = (key: string, redraw: () => void) => {
    mouse("mousedown", page.at(key), 1);
    mouse("mouseup", page.at(key), 1);
    redraw();
    mouse("mousedown", page.at(key), 2);
    redraw();
    mouse("mouseup", page.at(key), 2);
  };
  double("k-a", () => page.emit("agents.row", row("k-b", { ...card("k-b"), id: b.id, number: b.number })));
  expect(page.call).toHaveBeenCalledWith("agents.focus", { id: a.id });
  double("k-b", () => page.emit("agents.row", row("k-a", { id: a.id, number: a.number })));
  expect(page.call).toHaveBeenCalledWith("agents.round", { op: "start", key: "k-b" });
});

it("unknown_state_renders_by_class", async () => {
  const page = await open([row("k-a", { state: "pondering", class: "working", tool: null })]);
  expect(page.at("k-a").querySelector(".glyph")!.textContent).toBe("●");
  expect(page.at("k-a").querySelector(".what")!.textContent).toBe("working");
  expect(page.at("k-a").classList.contains("class-working")).toBe(true);
});

it("labels_are_text_nodes", async () => {
  const page = await open([row("k-a", { label: "<b>x</b>", project: { key: PROJECT.key, name: "<i>p</i>" } })]);
  expect(page.at("k-a").querySelector(".label")!.textContent).toBe("<b>x</b>");
  expect(page.root.querySelector(".project")!.textContent).toBe("<i>p</i>");
  expect(page.root.querySelector("b, i")).toBeNull();
});

it("agents_off_shows_the_off_line", async () => {
  const page = await open([row("k-a")], { enabled: false });
  expect(page.root.querySelector(".off")!.textContent).toBe("agents are off in this window");
  expect(page.slots()).toEqual([]);
});

const SHEET = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "strips.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
const RULES = [...SHEET.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(m => ({ selector: m[1].trim(), body: m[2] }));
const token = (body: string, prop: string) => body.match(new RegExp(`(?:^|[;\\s])${prop}:\\s*var\\((--nv-[a-z-]+)`))?.[1];

it("text_sits_on_the_fill_its_colour_is_guarded_against", () => {
  // fg, muted, cursorline and the signal colours are all guarded against the editor's bg; chrome is StatusLine's
  // fill, which a reversed StatusLine makes the very colour fg is.
  const page = RULES.find(r => r.selector === "html, body")!;
  expect(token(page.body, "background")).toBe("--nv-bg");
  expect(token(page.body, "color")).toBe("--nv-fg");
  const onChrome = RULES.filter(r => token(r.body, "background") === "--nv-chrome");
  for (const r of onChrome) expect(["--nv-chrome-fg", "--nv-chrome-muted"], r.selector).toContain(token(r.body, "color"));
});

it("a_window_and_tab_number_is_given_its_width", async () => {
  const other = { instance: "77aa00bb11cc22dd", name: "工作", focused: false };
  const page = await open([row("k-a", { number: 1 }), row("k-b", { window: other, number: 2 })]);
  expect(page.at("k-a").querySelector(".num")!.textContent).toBe("1·1");
  expect(page.at("k-b").querySelector(".num")!.textContent).toBe("工作·2");
  expect(page.at("k-b").querySelector<HTMLElement>(".num")!.title).toBe("工作·2");
  // Two cells for each wide character, one for the rest: the widest number in the list sets the column.
  expect(page.root.querySelector<HTMLElement>(".list")!.style.getPropertyValue("--num-w")).toBe("6.4ch");
  const grid = RULES.find(r => r.selector === ".row")!.body;
  expect(grid).toMatch(/grid-template-columns:\s*1\.4ch var\(--num-w, 2\.6ch\)/);
});

it("the_label_keeps_its_room_as_the_column_narrows", async () => {
  const page = await open([row("k-a", { label: "close question", tool: "Bash", state: "running", class: "working" })]);
  // Every row names itself whole on hover, wide or narrow, since a narrow column cuts it.
  expect(page.at("k-a").title).toBe("eitri 1·" + page.at("k-a").querySelector(".num")!.textContent + " close question: running");
  const sheet = SHEET;
  expect(RULES.find(r => r.selector === "#strips")!.body).toMatch(/container-type: inline-size;/);
  const blocks = [...sheet.matchAll(/@container \(max-width: (\d+)ch\) \{([\s\S]*?\})\s*\}/g)];
  const at = (n: number) => blocks.find(b => Number(b[1]) === n)?.[2] ?? "";
  // The tool word goes first, then the age; the label's column (the one fraction) stays in every template.
  expect(at(36)).toMatch(/\.row \.what \{ display: none; \}/);
  expect(at(24)).toMatch(/\.row \.what, \.row \.age \{ display: none; \}/);
  // The head keeps to one line: the count without the title, cut rather than wrapped.
  expect(at(24)).toMatch(/\.head \.title \{ display: none; \}/);
  expect(RULES.find(r => r.selector === ".head .sum")!.body).toMatch(/text-overflow: ellipsis;/);
  for (const n of [36, 24]) {
    const template = at(n).match(/grid-template-columns: ([^;]+);/)![1];
    expect(template).toContain("minmax(0, 1fr)");
    // One track per span still drawn: stop, number, label, glyph, (age,) edge.
    expect(template.split(/ (?![^(]*\))/)).toHaveLength(n === 36 ? 6 : 5);
  }
  // The narrow rail keeps its own template, more specific than the width rules, and at 62 px (about five and a half
  // cells) the count keeps two cells: glyph 1.4, count at least 2, a one-em tab, gaps and padding under one cell.
  const rail = RULES.find(r => r.selector === ".strips.narrow .row")!.body;
  expect(rail).toMatch(/grid-template-columns: 1\.4ch minmax\(2ch, 1fr\) 1em;/);
  expect(rail).toMatch(/column-gap: 0\.2ch;/);
  expect(rail).toMatch(/padding-left: 0\.3ch;/);
});

it("one_window_keeps_the_narrow_number_column", async () => {
  const page = await open([row("k-a", { number: 3 })]);
  expect(page.at("k-a").querySelector(".num")!.textContent).toBe("3");
  expect(page.root.querySelector<HTMLElement>(".list")!.style.getPropertyValue("--num-w")).toBe("");
});

// The page's clock and marks timers: they run only while something they would change is on screen.
async function idleOpen(rows: unknown[], opts: OpenOpts = {}) {
  const page = await open(rows, { ...opts, tick: true, now: () => Date.now() });
  const observer = new MutationObserver(() => {});
  observer.observe(page.root, { subtree: true, childList: true, characterData: true, attributes: true });
  const reads = () => page.call.mock.calls.filter(([verb]) => verb === "state.read").length;
  return { ...page, writes: () => observer.takeRecords().length, reads };
}

it("with_no_agents_nothing_is_written_and_no_timer_runs", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  try {
    vi.setSystemTime(NOW);
    const page = await idleOpen([], { marksEvery: 2_000 });
    expect(page.root.textContent).toContain("no agents");
    expect(vi.getTimerCount()).toBe(0);
    const reads = page.reads();
    page.writes();
    vi.advanceTimersByTime(20_000);
    expect(page.writes()).toBe(0);
    expect(page.reads()).toBe(reads);
    // An event that changes nothing drawn writes nothing.
    page.emit("agents.selection", null);
    expect(page.writes()).toBe(0);
  } finally {
    vi.useRealTimers();
  }
});

it("a_new_agent_starts_the_clock_and_an_unchanged_draw_writes_nothing", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  try {
    vi.setSystemTime(NOW);
    const page = await idleOpen([], { marksEvery: 0 });
    const a = row("k-a", { since: NOW - 5_400 });
    page.emit("agents.row", a);
    expect(page.at("k-a").querySelector(".age")!.textContent).toBe("5s");
    expect(vi.getTimerCount()).toBe(1);
    page.writes();
    // The same row again changes nothing on screen.
    page.emit("agents.row", a);
    vi.advanceTimersByTime(599);
    expect(page.writes()).toBe(0);
    vi.advanceTimersByTime(1);
    expect(page.at("k-a").querySelector(".age")!.textContent).toBe("6s");
    expect(page.writes()).toBeGreaterThan(0);
  } finally {
    vi.useRealTimers();
  }
});

it("an_age_in_minutes_waits_for_the_minute", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  try {
    vi.setSystemTime(NOW);
    const page = await idleOpen([row("k-a", { since: NOW - (20 * 60_000 + 30_000) })], { marksEvery: 0 });
    expect(page.at("k-a").querySelector(".age")!.textContent).toBe("20m");
    page.writes();
    vi.advanceTimersByTime(29_999);
    expect(page.writes()).toBe(0);
    vi.advanceTimersByTime(1);
    expect(page.at("k-a").querySelector(".age")!.textContent).toBe("21m");
  } finally {
    vi.useRealTimers();
  }
});

it("the_last_agent_gone_its_place_empties_on_time_then_every_timer_stops_until_one_comes", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  try {
    vi.setSystemTime(NOW);
    const a = row("k-a", { state: "new" });
    const page = await idleOpen([a], { marksEvery: 2_000 });
    // A new row shows no age: only the marks read runs.
    expect(vi.getTimerCount()).toBe(1);
    page.emit("agents.gone", { id: a.id, key: "k-a", why: "closed" });
    expect(page.slots()).toEqual([""]);
    // The held place turns empty when its hold ends, with no event behind it.
    await vi.advanceTimersByTimeAsync(5_000);
    expect(page.slots()).toEqual([]);
    expect(vi.getTimerCount()).toBe(0);
    const reads = page.reads();
    vi.advanceTimersByTime(20_000);
    expect(page.reads()).toBe(reads);
    // An agent comes: the marks are read at once (another window may have marked meanwhile) and again every 2 s.
    page.emit("agents.row", row("k-b", { state: "new" }));
    await vi.waitFor(() => expect(page.reads()).toBe(reads + 1));
    await vi.advanceTimersByTimeAsync(2_000);
    expect(page.reads()).toBe(reads + 2);
  } finally {
    vi.useRealTimers();
  }
});
