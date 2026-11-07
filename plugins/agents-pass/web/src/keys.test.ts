import { describe, expect, it, vi } from "vitest";
import { passKeys, type KeysKit } from "./keys";
import { fakeStore, round, row } from "./fixtures";

type InstallOptions = Parameters<KeysKit["install"]>[0];
function kit() {
  let opts: InstallOptions | undefined;
  const base: KeysKit = {
    install: vi.fn(async (o: InstallOptions) => { opts = o; return () => {}; }),
    message: vi.fn(),
  };
  return { base, installed: () => opts! };
}
const ev = new KeyboardEvent("keydown");

describe("the pass's keys", () => {
  it("n_advances_the_round", async () => {
    const fake = fakeStore([row("a", "card"), row("b", "card")], round("a", ["a"]));
    const call = vi.fn();
    const { base, installed } = kit();
    const shared = vi.fn(() => true);
    await passKeys(base, fake.store, call).install({ rows: [], run: shared });
    expect(installed().rows.some((r) => r.keys.includes("n") && r.action === "pass.next")).toBe(true);

    expect(installed().run("pass.next", ev)).toBe(true);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "next" });
    expect(shared).not.toHaveBeenCalled();

    // With no round running, n starts one at the oldest wait, as prefix u does.
    fake.set({ round: null });
    installed().run("pass.next", ev);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "start" });
  });

  it("esc_in_a_round_leaves_it_and_otherwise_is_the_shared_escape", async () => {
    const fake = fakeStore([row("a", "card")], round("a", ["a"]));
    const call = vi.fn();
    const { base, installed } = kit();
    const shared = vi.fn(() => true);
    await passKeys(base, fake.store, call).install({ rows: [], run: shared });
    installed().run("agents.escape", ev);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "leave" });
    expect(shared).not.toHaveBeenCalled();

    fake.set({ round: null });
    installed().run("agents.escape", ev);
    expect(shared).toHaveBeenCalledWith("agents.escape", ev);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it("a_finished_round_starts_afresh_on_n_and_leaves_esc_to_the_shared_keys", async () => {
    const fake = fakeStore([row("b", "card")], round(null, ["a"]));
    const call = vi.fn();
    const { base, installed } = kit();
    const shared = vi.fn(() => true);
    await passKeys(base, fake.store, call).install({ rows: [], run: shared });
    installed().run("pass.next", ev);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "start" });
    installed().run("agents.escape", ev);
    expect(shared).toHaveBeenCalledWith("agents.escape", ev);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it("enter_with_no_round_starts_it_at_the_oldest_wait", async () => {
    // Another page selected b; the pass shows only the oldest wait, a, and Enter starts the round there.
    const rows = [row("b", "card", { waiting_since: 2_000 }), row("a", "card", { waiting_since: 1_000 })];
    const fake = fakeStore(rows, null, "b");
    const call = vi.fn();
    const { base, installed } = kit();
    const shared = vi.fn(() => true);
    await passKeys(base, fake.store, call).install({ rows: [], run: shared });
    expect(installed().run("agents.open", ev)).toBe(true);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "start", key: "a" });
    expect(shared).not.toHaveBeenCalled();

    // A finished round (no current stop) is no round either.
    fake.set({ round: round(null, ["x"]) });
    installed().run("agents.open", ev);
    expect(call).toHaveBeenLastCalledWith("agents.round", { op: "start", key: "a" });

    // Nothing waits: Enter is the shared key's (it goes to the selected row's chat).
    fake.set({ rows: [row("b", "idle")], round: null });
    installed().run("agents.open", ev);
    expect(shared).toHaveBeenCalledWith("agents.open", ev);
    expect(call).toHaveBeenCalledTimes(2);
  });

  it("the_shared_keys_pass_through_unchanged", async () => {
    const fake = fakeStore([row("a", "card")], round("a", ["a"]));
    const { base, installed } = kit();
    const shared = vi.fn(() => true);
    const rows = [{ action: "agents.next", keys: ["j"], context: "Agents", desc: "Next agent" }];
    await passKeys(base, fake.store, vi.fn()).install({ rows, run: shared });
    expect(installed().rows[0]).toEqual(rows[0]);
    installed().run("agents.open", ev);
    expect(shared).toHaveBeenCalledWith("agents.open", ev);
  });
});
