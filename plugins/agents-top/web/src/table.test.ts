import { afterEach, expect, it, vi } from "vitest";
import { createStore } from "../../../../plugin-kit/agents/src/store";
import type { Row } from "../../../../plugin-kit/agents/src/store";
import { glyph, word } from "../../../../plugin-kit/agents/src/glyphs";
import { sortRows } from "./sort";
import { createTable } from "./table";

const row = (key: string, values: Partial<Row> = {}): Row => ({
  id: `instance:${key}`, key, agent: "claude", project: { key: "p", name: "project" },
  window: { instance: "instance", name: "1", focused: false }, number: 1,
  label: key, label_from: "user", shown: false, state: "idle", class: "quiet", since: 100,
  needs: { cards: 0, questions: 0, trust: false }, waiting_since: null, unread: false, tool: null,
  background: { calls: 0, shells: 0, monitors: 0, agents: 0, other: 0 }, queued: 0, mode: "auto",
  ended_how: null, moving: false, ...values,
});
const cleanups: Array<() => void> = [];
afterEach(() => { cleanups.splice(0).forEach(fn => fn()); document.body.replaceChildren(); });
function setup(rows: Row[], visited: string[] = []) {
  const { store, apply } = createStore();
  apply("agents.reset", { rows, selection: { key: rows[0]?.key }, round: {
    current: rows[1]?.key ?? null, visited, started_at: 0, at: 0,
    from: { instance: "instance", module: "editor", tab: null },
  } });
  const root = document.createElement("main");
  document.body.append(root);
  const table = createTable(root, { store, glyph, word });
  cleanups.push(table.destroy);
  return { root, table, apply };
}

it("sorts_by_state_age_project_label", () => {
  const rows = [row("idle"), row("running", { state: "running", class: "working" }),
    row("question", { state: "question", class: "needs" }), row("card", { state: "card", class: "needs" })];
  expect(sortRows(rows, "state").map(r => r.key)).toEqual(["card", "question", "running", "idle"]);
  expect(sortRows([row("young", { since: 200 }), row("old", { since: 10 })], "age").map(r => r.key))
    .toEqual(["old", "young"]);
  expect(sortRows([row("z", { project: { key: "z", name: "zeta" } }), row("a")], "project").map(r => r.key))
    .toEqual(["a", "z"]);
  expect(sortRows([row("z"), row("a")], "label").map(r => r.key)).toEqual(["a", "z"]);
  expect(rows.map(r => r.key)).toEqual(["idle", "running", "question", "card"]);
});
it("S_reverses", () => {
  const { table } = setup([row("a"), row("b")]);
  table.setSort("label");
  expect(table.order()).toEqual(["a", "b"]);
  table.handleKey("S");
  expect(table.order()).toEqual(["b", "a"]);
  table.handleKey("S");
  expect(table.order()).toEqual(["a", "b"]);
  table.handleKey("s");
  expect(table.sort()).toBe("state");
});
it("filter_hides_rows_not_matching", () => {
  const { root, table } = setup([row("first", { label: "Fix Parser" }), row("second")]);
  table.setFilter("PARSER");
  expect(table.order()).toEqual(["first"]);
  expect(root.querySelectorAll("tbody tr")).toHaveLength(1);
  expect(root.querySelector("tbody")?.textContent).not.toContain("second");
  table.setFilter("");
  expect(table.order()).toHaveLength(2);
});
it("the_header_counts_waiting_by_class", () => {
  const { root, table } = setup([row("future", { state: "future-wait", class: "needs" }),
    row("question", { state: "question", class: "needs" }), row("idle")]);
  expect(root.querySelector("h1")?.textContent).toBe("eitri agents — 3 agents, 2 waiting");
  table.setFilter("idle");
  expect(root.querySelector("h1")?.textContent).toBe("eitri agents — 3 agents, 2 waiting");
});
it("unknown_state_sorts_by_class", () => {
  const rows = [row("quiet", { state: "future-quiet" }),
    row("working", { state: "future-work", class: "working" }),
    row("needs", { state: "future-wait", class: "needs" }),
    row("ended", { state: "future-end", class: "ended" })];
  expect(sortRows(rows, "state").map(r => r.key)).toEqual(["needs", "working", "quiet", "ended"]);
  const { root } = setup(rows);
  expect(root.querySelector('[data-key="needs"] [data-column="state"]')?.textContent).toContain("⚑ needs");
});
it("visited_round_rows_are_marked", () => {
  const { root, apply } = setup([row("a"), row("b")], ["a"]);
  expect(root.querySelector('[data-key="a"]')?.classList.contains("visited")).toBe(true);
  expect(root.querySelector('[data-key="b"]')?.classList.contains("current")).toBe(true);
  apply("agents.round", null);
  expect(root.querySelectorAll(".visited, .current")).toHaveLength(0);
});
it("labels_are_text_nodes", () => {
  const label = '<img src=x onerror="alert(1)"><script>bad()</script>';
  const { root } = setup([row("markup", { label })]);
  const cell = root.querySelector('[data-column="label"]')!;
  expect(cell.textContent).toBe(label);
  expect(cell.childNodes).toHaveLength(1);
  expect(cell.firstChild?.nodeType).toBe(Node.TEXT_NODE);
  expect(root.querySelector("img, script")).toBeNull();
});
it("failed_and_ended_rows_follow_the_needs_states", () => {
  const rows = [row("idle"), row("running", { state: "running", class: "working" }),
    row("ended", { state: "ended", class: "ended" }), row("failed", { state: "failed", class: "ended" }),
    row("trust", { state: "trust", class: "needs" }), row("new", { state: "new" }),
    row("done", { state: "done" })];
  expect(sortRows(rows, "state").map(r => r.key))
    .toEqual(["trust", "failed", "ended", "running", "done", "idle", "new"]);
  const unknown = [row("quiet", { state: "future-quiet" }), row("end", { state: "future-end", class: "ended" }),
    row("work", { state: "future-work", class: "working" }), row("failed", { state: "failed", class: "ended" }),
    row("running", { state: "running", class: "working" })];
  expect(sortRows(unknown, "state").map(r => r.key)).toEqual(["failed", "running", "work", "quiet", "end"]);
});
it("a_redraw_keeps_the_row_elements_and_their_clicks", () => {
  const picked: string[] = [];
  const { store, apply } = createStore();
  apply("agents.reset", { rows: [row("a"), row("b")], selection: { key: "a" }, round: null });
  const root = document.createElement("main");
  document.body.append(root);
  const table = createTable(root, { store, glyph, word }, key => picked.push(key));
  cleanups.push(table.destroy);
  const before = root.querySelector('[data-key="b"]')!;
  apply("agents.row", row("b", { label: "renamed" }));
  table.setSort("label");
  const after = root.querySelector('[data-key="b"]')!;
  expect(after).toBe(before);
  expect(after.querySelector('[data-column="label"]')?.textContent).toBe("renamed");
  (before as HTMLElement).click();
  expect(picked).toEqual(["b"]);
  apply("agents.gone", { key: "b", id: "instance:b" });
  expect(root.querySelector('[data-key="b"]')).toBeNull();
  expect(root.querySelectorAll("tbody tr")).toHaveLength(1);
});
it("the_selected_row_scrolls_into_view_when_the_selection_moves", () => {
  const scroll = vi.fn();
  const original = Element.prototype.scrollIntoView;
  Element.prototype.scrollIntoView = scroll;
  cleanups.push(() => { Element.prototype.scrollIntoView = original; });
  const { apply } = setup([row("a"), row("b"), row("c")]);
  scroll.mockClear();
  apply("agents.selection", { key: "c" });
  expect(scroll).toHaveBeenCalledTimes(1);
  expect(scroll.mock.contexts[0]).toBe(document.querySelector('[data-key="c"]'));
  expect(scroll.mock.calls[0][0]).toEqual({ block: "nearest" });
  apply("agents.row", row("a", { label: "other" }));
  expect(scroll).toHaveBeenCalledTimes(1);
});
