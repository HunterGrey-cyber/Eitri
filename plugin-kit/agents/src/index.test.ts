import { beforeEach, expect, it, vi } from "vitest";
import watch from "../../../core/src/plugin/fixtures/rep-agents-watch.json";

type Listener = (data: unknown) => void;
let listeners: Map<string, Set<Listener>>;
let call: ReturnType<typeof vi.fn>;
let install: ReturnType<typeof vi.fn>;
let kit: typeof import("./index").eitriAgents;
const original = watch.result.rows[0];
const row = (key = original.key, patch = {}) => ({ ...original, key, ...patch });
const view = (rows = [row()]) => ({ ...watch.result, rows });
const emit = (event: string, data: unknown) => listeners.get(event)?.forEach(fn => fn(data));

beforeEach(async () => {
  vi.resetModules();
  listeners = new Map();
  call = vi.fn(async (verb: string) => verb === "host.hello"
    ? { capabilities: ["agents", "layout.focus"], timeoutlen: 500, prefix: "<C-a>", keys_settings: {} }
    : verb === "agents.watch" ? view() : {});
  install = vi.fn(async () => () => {});
  Object.defineProperty(window, "eitriPlugin", { configurable: true, value: {
    call, on(event: string, fn: Listener) {
      if (!listeners.has(event)) listeners.set(event, new Set());
      listeners.get(event)!.add(fn);
      return () => { listeners.get(event)?.delete(fn); };
    },
  } });
  Object.defineProperty(window, "eitriKeys", { configurable: true, value: { install, message: vi.fn() } });
  kit = (await import("./index")).eitriAgents;
});

it("the_store_applies_every_change_type_and_ignores_an_unknown_one", async () => {
  await kit.start();
  const changed = vi.fn();
  const off = kit.store.on(changed);
  emit("agents.row", row("k-second", { number: 1 }));
  expect(kit.store.rows().map(r => r.key)).toEqual(["k-second", original.key]);
  emit("agents.row", row(original.key, { label: "renamed" }));
  expect(kit.store.byKey(original.key)?.label).toBe("renamed");
  emit("agents.selection", { key: "k-second", at: 1, by: "window" });
  expect(kit.store.selection()).toBe("k-second");
  emit("agents.round", watch.result.round);
  expect(kit.store.round()).toEqual(watch.result.round);
  emit("agents.gone", { id: original.id, key: original.key, why: "closed" });
  expect(kit.store.byKey(original.key)).toBeUndefined();
  emit("agents.reset", view());
  expect(kit.store.rows()).toEqual([original]);
  expect(changed).toHaveBeenCalledTimes(6);
  emit("agents.future", { key: "k-future" });
  expect(changed).toHaveBeenCalledTimes(6);
  off();
  emit("agents.selection", null);
  expect(kit.store.selection()).toBeNull();
  expect(changed).toHaveBeenCalledTimes(6);
  emit("agents.round", null);
  expect(kit.store.round()).toBeNull();
});

it("reset_replaces_everything", async () => {
  await kit.start();
  emit("agents.reset", { ...view([row("k-other")]), selection: null, round: null });
  expect(kit.store.rows().map(r => r.key)).toEqual(["k-other"]);
  expect(kit.store.byKey(original.key)).toBeUndefined();
  expect(kit.store.selection()).toBeNull();
  expect(kit.store.round()).toBeNull();
});

it("a_gone_moved_row_reappears_under_its_key", async () => {
  await kit.start();
  emit("agents.gone", { id: original.id, key: original.key, why: "moved" });
  expect(kit.store.rows()).toEqual([]);
  const moved = row(original.key, { id: "new-window.8" });
  emit("agents.row", moved);
  expect(kit.store.rows()).toEqual([moved]);
  emit("agents.gone", { id: original.id, key: original.key, why: "moved" });
  expect(kit.store.byKey(original.key)?.id).toBe("new-window.8");
});

it("unknown_state_falls_back_to_the_class_glyph", () => {
  for (const [cls, glyph] of [["needs", "⚑"], ["working", "●"], ["quiet", "·"], ["ended", "✕"]]) {
    const unknown = row(original.key, { state: "future", class: cls });
    expect(kit.glyph(unknown)).toBe(glyph);
    expect(kit.word(unknown)).toBe(cls);
  }
  expect(kit.glyph(original)).toBe("⚑");
  expect(kit.word(original)).toBe("card");
});

async function bind(order = () => kit.store.rows().map(r => r.key)) {
  await kit.bindKeys({ order });
  call.mockClear();
  const opts = install.mock.calls[0][0];
  return (key: string) => {
    const action = opts.rows.find((r: { keys: string[] }) => r.keys.includes(key))?.action;
    expect(action).toBeDefined();
    return opts.run(action, new KeyboardEvent("keydown", { key }));
  };
}

it("enter_on_a_needs_row_starts_the_round_there", async () => {
  const press = await bind();
  press("<CR>");
  expect(call).toHaveBeenCalledWith("agents.round", { op: "start", key: original.key });
});

it("enter_on_a_quiet_row_focuses", async () => {
  const press = await bind();
  emit("agents.row", row(original.key, { class: "quiet", state: "idle" }));
  press("<CR>");
  expect(call).toHaveBeenCalledWith("agents.focus", { id: original.id });
});

it("j_k_write_the_selection", async () => {
  const press = await bind(() => [original.key, "k-next", "k-last"]);
  press("j");
  expect(call).toHaveBeenLastCalledWith("agents.select", { key: "k-next" });
  emit("agents.selection", { key: "k-next" });
  press("k");
  expect(call).toHaveBeenLastCalledWith("agents.select", { key: original.key });
  expect(kit.store.selection()).toBe("k-next");
});

it("x_asks_stop", async () => {
  const press = await bind();
  press("x");
  expect(call).toHaveBeenCalledWith("agents.stop", { id: original.id });
});

it("c_asks_new_in_the_selected_rows_project", async () => {
  const press = await bind();
  press("c");
  expect(call).toHaveBeenCalledWith("agents.new", { project: original.project.key });
});

it("start_calls_hello_then_watch", async () => {
  const hello = Promise.withResolvers<unknown>();
  call.mockImplementationOnce(() => hello.promise);
  const starting = kit.start();
  expect(call.mock.calls).toEqual([["host.hello", {}]]);
  hello.resolve({ capabilities: ["agents"] });
  await starting;
  expect(call.mock.calls).toEqual([["host.hello", {}], ["agents.watch", {}]]);
  expect(kit.store.rows()).toEqual([original]);
  expect(Object.isFrozen(window.eitriAgents)).toBe(true);
  expect(window.eitriAgents.version).toBe(1);
});

it("escape_focuses_the_editor_only_with_the_declared_capability", async () => {
  const press = await bind();
  press("<Esc>");
  expect(call).toHaveBeenCalledWith("layout.focus", { target: "editor" });
});

it("escape_without_layout_focus_sends_nothing", async () => {
  call.mockImplementationOnce(async () => ({ capabilities: ["agents"] }));
  const press = await bind();
  press("<Esc>");
  expect(call).not.toHaveBeenCalled();
});

it("filter_reports_text_and_closes_without_a_host_call", async () => {
  const onFilter = vi.fn();
  const remove = await kit.bindKeys({ order: () => [], onFilter });
  call.mockClear();
  const opts = install.mock.calls[0][0];
  opts.run("agents.filter", new KeyboardEvent("keydown", { key: "/" }));
  const input = document.querySelector<HTMLInputElement>('input[aria-label="Filter agents"]')!;
  expect(document.activeElement).toBe(input);
  input.value = "claude";
  input.dispatchEvent(new Event("input"));
  expect(onFilter).toHaveBeenLastCalledWith("claude");
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(document.contains(input)).toBe(false);
  expect(call).not.toHaveBeenCalled();
  remove();
});

it("empty_selection_does_not_act_on_an_unselected_row", async () => {
  const press = await bind();
  emit("agents.selection", null);
  for (const key of ["<CR>", "c", "x"]) press(key);
  expect(call).not.toHaveBeenCalled();
  press("j");
  expect(call).toHaveBeenLastCalledWith("agents.select", { key: original.key });
});

it("events_during_watch_are_applied_after_the_initial_view", async () => {
  call.mockImplementation(async (verb: string) => {
    if (verb === "host.hello") return { capabilities: ["agents"] };
    emit("agents.row", row(original.key, { label: "new label" }));
    emit("agents.selection", null);
    return view();
  });
  await kit.start();
  expect(kit.store.byKey(original.key)?.label).toBe("new label");
  expect(kit.store.selection()).toBeNull();
});

it("a_failed_watch_can_be_started_again_without_duplicate_listeners", async () => {
  call.mockImplementationOnce(async () => ({ capabilities: ["agents"] }));
  call.mockRejectedValueOnce(new Error("unavailable"));
  await expect(kit.start()).rejects.toThrow("unavailable");
  await kit.start();
  expect(listeners.get("agents.row")?.size).toBe(1);
  call.mockClear();
  await kit.start();
  expect(call).not.toHaveBeenCalled();
});

it("a_face_cannot_mutate_the_stores_rows_or_round", async () => {
  await kit.start();
  kit.store.rows()[0].project.name = "changed";
  kit.store.byKey(original.key)!.label = "changed";
  kit.store.round()!.visited.push("changed");
  expect(kit.store.rows()).toEqual([original]);
  expect(kit.store.round()).toEqual(watch.result.round);
});
