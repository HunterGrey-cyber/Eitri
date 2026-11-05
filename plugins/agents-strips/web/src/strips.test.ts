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

async function open(rows: unknown[], opts: { enabled?: boolean; settings?: Record<string, unknown>; store?: { value: unknown } } = {}) {
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
  const { boot } = await import("./app");
  const page = await boot(document, window, { now: () => NOW, tick: false });
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
  expect(page.root.querySelector<HTMLElement>(".list")!.style.getPropertyValue("--num-w")).toBe("6ch");
  const grid = RULES.find(r => r.selector === ".row")!.body;
  expect(grid).toMatch(/grid-template-columns:\s*1\.4ch var\(--num-w, 2\.6ch\)/);
});

it("one_window_keeps_the_narrow_number_column", async () => {
  const page = await open([row("k-a", { number: 3 })]);
  expect(page.at("k-a").querySelector(".num")!.textContent).toBe("3");
  expect(page.root.querySelector<HTMLElement>(".list")!.style.getPropertyValue("--num-w")).toBe("");
});
