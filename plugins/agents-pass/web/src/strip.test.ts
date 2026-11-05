import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { age, mountStrip, place, tickets, wanted } from "./strip";
import { fakeStore, glyph, round, row } from "./fixtures";

const NOW = 200_000;
function mount(store: ReturnType<typeof fakeStore>["store"]) {
  const root = document.createElement("div");
  document.body.replaceChildren(root);
  const strip = mountStrip(root, { store, glyph, now: () => NOW, every: () => () => {} });
  return { root, strip };
}
const shown = (root: HTMLElement) => [...root.querySelectorAll<HTMLElement>(".ticket")].map((t) => t.dataset.key);

describe("the pass", () => {
  it("tickets_are_needs_rows_oldest_first", () => {
    const rows = [
      row("a", "running"),
      row("b", "card", { waiting_since: 3_000 }),
      row("c", "question", { waiting_since: 1_000 }),
      row("d", "idle"),
      row("e", "trust", { waiting_since: 2_000 }),
    ];
    expect(tickets(rows, null, null).map((t) => t.row.key)).toEqual(["c", "e", "b"]);
    const { root, strip } = mount(fakeStore(rows, round("c", ["c"])).store);
    expect(shown(root)).toEqual(["c", "e", "b"]);
    expect(strip.order()).toEqual(["c", "e", "b"]);
  });

  it("the_current_stop_is_marked_with_its_place", () => {
    const rows = [
      row("a", "card", { waiting_since: 1_000, number: 1, label: "close question" }),
      row("b", "question", { waiting_since: 2_000 }),
      row("c", "card", { waiting_since: 3_000 }),
    ];
    expect(place(rows, round("a", ["a"]))).toEqual({ at: 1, of: 3 });
    const fake = fakeStore(rows, round("a", ["a"]));
    const { root } = mount(fake.store);
    expect(root.querySelector(".lead")?.textContent).toBe("round 1/3");
    const current = root.querySelectorAll(".current");
    expect(current.length).toBe(1);
    expect((current[0] as HTMLElement).dataset.key).toBe("a");
    expect(current[0].textContent).toBe("▸ ⚑ eitri·1 close question 3m19");

    // The first stop answered: it no longer waits, and the round stands at its second stop of three.
    fake.set({ rows: [row("a", "idle"), rows[1], rows[2]], round: round("b", ["a", "b"]) });
    expect(place(fake.store.rows(), fake.store.round())).toEqual({ at: 2, of: 3 });
    expect(root.querySelector(".lead")?.textContent).toBe("round 2/3");
    expect(shown(root)).toEqual(["b", "c"]);
    expect((root.querySelector(".current") as HTMLElement).dataset.key).toBe("b");
  });

  it("a_window_s_trust_rows_are_one_stop", () => {
    const rows = [
      row("a", "trust", { waiting_since: 1_000 }),
      row("b", "trust", { waiting_since: 2_000 }),
      row("c", "card", { waiting_since: 3_000, window: { instance: "w2", name: "2" } }),
    ];
    expect(place(rows, round("a", ["a", "b"]))).toEqual({ at: 1, of: 2 });
  });

  it("no_round_shows_counts_and_the_prefix_u_hint", () => {
    const rows = [row("a", "card"), row("b", "question"), row("c", "card"), row("d", "running")];
    const { root } = mount(fakeStore(rows).store);
    expect(root.hidden).toBe(false);
    expect(shown(root)).toEqual([]);
    const counts = [...root.querySelectorAll(".count")].map((c) => c.textContent);
    expect(counts).toEqual(["⚑ 2", "? 1"]);
    expect(root.querySelector(".hint")?.textContent).toBe("waiting · prefix u to start");
  });

  it("nothing_waiting_and_no_round_hides_the_strip", () => {
    const { root } = mount(fakeStore([row("a", "running"), row("b", "idle"), row("c", "failed")]).store);
    expect(root.hidden).toBe(true);
    expect(root.childElementCount).toBe(0);

    // A round that runs keeps the strip up, even with nothing left waiting.
    const running = mount(fakeStore([row("a", "idle")], round("a", ["a"])).store);
    expect(running.root.hidden).toBe(false);
    expect(running.root.querySelector(".hint")?.textContent).toBe("n next · prefix B back");
  });

  it("a_card_arriving_shows_the_strip", () => {
    const fake = fakeStore([row("a", "running")]);
    const { root } = mount(fake.store);
    expect(root.hidden).toBe(true);
    fake.set({ rows: [row("a", "card")] });
    expect(root.hidden).toBe(false);
    expect(root.querySelector(".count")?.textContent).toBe("⚑ 1");
    fake.set({ rows: [row("a", "running")] });
    expect(root.hidden).toBe(true);
  });

  it("a_round_whose_stop_went_is_not_a_running_one", () => {
    // The current stop's window was killed: its row went, the round file still names it.
    const orphaned = round("gone", ["gone"]);
    expect(wanted([row("a", "idle")], orphaned)).toBe(false);
    const { root } = mount(fakeStore([row("a", "idle")], orphaned).store);
    expect(root.hidden).toBe(true);
  });

  it("a_finished_round_is_not_a_running_one", () => {
    // A round that ended with the person in it is kept with no current stop, only so prefix B can go back.
    const ended = round(null, ["a"]);
    expect(wanted([row("a", "idle")], ended)).toBe(false);
    const fake = fakeStore([row("a", "idle")], ended);
    const { root } = mount(fake.store);
    expect(root.hidden).toBe(true);
    // Something waits again: the strip counts it and says how to start a fresh round.
    fake.set({ rows: [row("a", "idle"), row("b", "card")] });
    expect(root.hidden).toBe(false);
    expect(root.querySelector(".lead")).toBeNull();
    expect(shown(root)).toEqual([]);
    expect(root.querySelector(".hint")?.textContent).toBe("waiting · prefix u to start");
  });

  it("a_window_s_trust_stop_stays_one_stop_after_it_is_answered", () => {
    const rows = [
      row("a", "trust", { waiting_since: 1_000 }),
      row("b", "trust", { waiting_since: 2_000 }),
      row("c", "card", { waiting_since: 3_000, window: { instance: "w2", name: "2" } }),
    ];
    const fake = fakeStore(rows, round("a", ["a", "b"]));
    const { root } = mount(fake.store);
    expect(root.querySelector(".lead")?.textContent).toBe("round 1/2");
    // One answer settles the window's trust: both rows stop waiting, and the round moves on to c.
    fake.set({ rows: [row("a", "running"), row("b", "running"), rows[2]], round: round("c", ["a", "b", "c"]) });
    expect(root.querySelector(".lead")?.textContent).toBe("round 2/2");
  });

  it("labels_are_text_nodes", () => {
    const evil = row("a", "card", { label: "<b>x</b><img src=x onerror=alert(1)>" });
    const { root } = mount(fakeStore([evil], round("a", ["a"])).store);
    expect(root.querySelector("b")).toBeNull();
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector(".label")?.textContent).toBe("<b>x</b><img src=x onerror=alert(1)>");
  });

  it("the_shared_selection_is_marked", () => {
    const rows = [row("a", "card", { waiting_since: 1_000 }), row("b", "card", { waiting_since: 2_000 })];
    const fake = fakeStore(rows, round("a", ["a"]), "b");
    const { root } = mount(fake.store);
    expect([...root.querySelectorAll<HTMLElement>(".selected")].map((t) => t.dataset.key)).toEqual(["b"]);
  });

  it("tickets_name_the_window_when_rows_come_from_more_than_one", () => {
    const two = { instance: "w2", name: "2" };
    const rows = [
      row("a", "card", { waiting_since: 1_000, number: 1, label: "" }),
      row("b", "card", { waiting_since: 2_000, number: 1, label: "", window: two }),
    ];
    const fake = fakeStore(rows, round("a", ["a"]));
    const { root } = mount(fake.store);
    const places = () => [...root.querySelectorAll(".ticket .where")].map((w) => w.textContent);
    // Tab 1 of window 1 and tab 1 of window 2 read apart.
    expect(places()).toEqual(["eitri 1·1", "eitri 2·1"]);
    // One window: the tab alone, as before.
    fake.set({ rows: [rows[0], row("c", "card", { waiting_since: 3_000, number: 2, label: "" })] });
    expect(places()).toEqual(["eitri·1", "eitri·2"]);
    // A quiet row of another window counts: its tab 1 is somewhere the person can see.
    fake.set({ rows: [rows[0], row("d", "running", { window: two })] });
    expect(places()).toEqual(["eitri 1·1"]);
  });

  it("a_narrow_module_keeps_the_current_ticket_and_counts_the_others", () => {
    const rows = [
      row("a", "card", { waiting_since: 1_000 }),
      row("b", "question", { waiting_since: 2_000 }),
      row("c", "card", { waiting_since: 3_000 }),
    ];
    const fake = fakeStore(rows, round("b", ["a", "b"]));
    const { root } = mount(fake.store);
    expect(root.querySelector(".more")?.textContent).toBe("+2");
    // The count comes after the tickets, before the hint, so a narrow module shows `round 2/3 ▸ ? … +2`.
    const order = [...root.children].map((c) => c.className);
    expect(order).toEqual(["lead", "tickets", "more", "hint"]);
    // Alone, the current ticket has nothing to count.
    fake.set({ rows: [rows[1]], round: round("b", ["a", "b"]) });
    expect(root.querySelector(".more")).toBeNull();
    // The current stop answered (its row no longer waits) and `n` not pressed yet: the round still runs, and a narrow
    // strip, which hides every ticket but the current one, still says how many wait.
    fake.set({ rows: [rows[0], row("b", "running"), rows[2]], round: round("b", ["a", "b"]) });
    expect(root.querySelector(".current")).toBeNull();
    expect(root.querySelector(".more")?.textContent).toBe("+2");
  });

  it("the_stylesheet_gives_a_narrow_module_the_current_ticket", () => {
    const css = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "pass.css"), "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "");
    const narrow = css.match(/@container \(max-width: (\d+)ch\) \{([\s\S]*?\})\s*\}/);
    expect(narrow).not.toBeNull();
    // Hidden there: every ticket but the current one, the separators and the round's hint (the one after the
    // tickets; with no round the hint follows the counts and stays); shown: the count.
    expect(narrow![2]).toMatch(/\.ticket:not\(\.current\), \.sep, \.tickets ~ \.hint \{ display: none; \}/);
    expect(narrow![2]).toMatch(/\.more \{ display: inline; \}/);
    // The body is the container the rule measures.
    expect(css).toMatch(/body \{\s*container-type: inline-size;/);
    // A ticket's label is the one part that gives way.
    expect(css).toMatch(/\.ticket > \.label \{[^}]*text-overflow: ellipsis;/);
  });

  it("ages_read_as_the_command_line_prints_them", () => {
    expect(age(41_000, 0)).toBe("41s");
    expect(age(180_000, 0)).toBe("3m");
    expect(age(95_000, 0)).toBe("1m35");
    expect(age(4_320_000, 0)).toBe("1h12");
    expect(age(0, 5_000)).toBe("0s");
  });
});
