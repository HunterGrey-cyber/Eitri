// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { install, setCurrent } from "./install";
import { GENERIC_ROWS, type Row } from "./rows";
import "./index";

const pageRows: Row[] = [
  { action: "item.next", keys: ["]q"], context: "阅读", desc: "下一题" },
  { action: "item.pick", keys: ["1"], context: "题目", desc: "选择" },
];
let events: Map<string, (data: unknown) => void>;
let effective: Row[];
let call: ReturnType<typeof vi.fn>;
let cleanup: (() => void) | undefined;

function key(key: string, init: KeyboardEventInit = {}, target: EventTarget = document.body) {
  const ev = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(ev);
  return ev;
}

beforeEach(() => {
  vi.useFakeTimers();
  events = new Map();
  effective = [...GENERIC_ROWS, ...pageRows].map((row) => ({ ...row, keys: [...row.keys] }));
  call = vi.fn(async () => ({ rows: effective }));
  window.eitriPlugin = {
    call,
    on: (name, fn) => {
      events.set(name, fn);
      return () => { events.delete(name); };
    },
  };
  vi.spyOn(window, "scrollBy").mockImplementation(() => {});
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

async function start(run = vi.fn(() => true), initial?: { timeoutlen: number; hasKeys: boolean }) {
  cleanup = await install({ rows: pageRows, run, initial });
  return run;
}

describe("installation", () => {
  it("passes the page's labels to the in-page sheet", async () => {
    cleanup = await install({ rows: pageRows, run: vi.fn(() => true), labels: { sheetTitle: "按键" } });
    key("?", { shiftKey: true });
    expect(document.querySelector("#eitri-keys-sheet h2")!.textContent).toBe("按键");
    key("Escape");
  });

  it("publishes the versioned API", () => {
    expect(window.eitriKeys).toMatchObject({ version: 2, install, setCurrent });
    for (const name of ["message", "status", "keys"] as const) expect(typeof window.eitriKeys[name]).toBe("function");
  });

  it("declares merged rows without owning host.hello", async () => {
    await start();
    expect(call.mock.calls).toEqual([["keys.declare", { rows: [...GENERIC_ROWS, ...pageRows] }]]);
    expect([...events.keys()]).toEqual(["focus", "keys_config"]);
  });

  it("honors the effective control chord added by the host", async () => {
    effective.find((row) => row.action === "scroll.down")!.keys.push("<C-n>");
    await start();
    expect(key("n", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(window.scrollBy).toHaveBeenCalledWith({ top: 60, behavior: "smooth" });
  });

  it("reads the host's key/source pairs in effective rows", async () => {
    call.mockResolvedValueOnce({ rows: effective.map((row) => ({
      ...row, keys: row.action === "scroll.down" ? [["<C-n>", "init.lua"]] : row.keys.map((key) => [key, "default"]),
    })) });
    await start();
    key("n", { ctrlKey: true });
    expect(window.scrollBy).toHaveBeenCalledWith({ top: 60, behavior: "smooth" });
  });

  it("does not retain a binding removed by the host", async () => {
    effective.find((row) => row.action === "scroll.down")!.keys = [];
    await start();
    expect(key("j").defaultPrevented).toBe(false);
    expect(window.scrollBy).not.toHaveBeenCalled();
  });

  it("dims and hides the HUD and current ring while the page lacks the keys", async () => {
    await start();
    const current = document.createElement("div");
    document.body.append(current);
    setCurrent(current);
    events.get("focus")!({ has_keys: false });
    const hud = document.querySelector<HTMLElement>("#eitri-keys-hud")!;
    expect(hud.hidden).toBe(true);
    expect(hud.dataset.dimmed).toBe("true");
    expect(current.classList.contains("eitri-keys-dimmed")).toBe(true);
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
    events.get("focus")!({ has_keys: true });
    expect(hud.hidden).toBe(false);
    expect(current.classList.contains("eitri-keys-dimmed")).toBe(false);
  });

  it("applies keys_config timeout changes to later prefixes", async () => {
    const run = await start();
    events.get("keys_config")!({ timeoutlen: 300 });
    key("]");
    vi.advanceTimersByTime(301);
    key("q");
    expect(run).not.toHaveBeenCalled();
    key("]");
    vi.advanceTimersByTime(299);
    key("q");
    expect(run).toHaveBeenCalledWith("item.next", expect.any(KeyboardEvent));
  });

  it("keeps the latest focus event received while declaration is pending", async () => {
    let reply!: (value: unknown) => void;
    call.mockImplementationOnce(() => new Promise((resolve) => { reply = resolve; }));
    const starting = start();
    events.get("focus")?.({ has_keys: true });
    events.get("focus")?.({ has_keys: false });
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
    reply({ rows: effective });
    await starting;
    expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.hidden).toBe(true);
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
    events.get("focus")!({ has_keys: true });
    key("j");
    expect(window.scrollBy).toHaveBeenCalledTimes(1);
  });

  it("uses configuration received during declaration with the effective bindings", async () => {
    let reply!: (value: unknown) => void;
    call.mockImplementationOnce(() => new Promise((resolve) => { reply = resolve; }));
    const run = vi.fn(() => true);
    const starting = start(run);
    events.get("keys_config")?.({ timeoutlen: 800 });
    events.get("keys_config")?.({ timeoutlen: 300 });
    effective.find((row) => row.action === "item.next")!.keys = ["zx"];
    reply({ rows: effective });
    await starting;
    key("z");
    vi.advanceTimersByTime(301);
    key("x");
    expect(run).not.toHaveBeenCalled();
    key("z");
    vi.advanceTimersByTime(299);
    key("x");
    expect(run).toHaveBeenCalledWith("item.next", expect.any(KeyboardEvent));
  });

  it("completes gT across the Shift keydown", async () => {
    effective.push({ action: "item.previous", keys: ["gT"], context: "阅读", desc: "上一题" });
    const run = await start();
    key("g");
    expect(key("Shift", { shiftKey: true }).defaultPrevented).toBe(false);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).toContain("g");
    const ev = key("T", { shiftKey: true });
    expect(run).toHaveBeenCalledWith("item.previous", ev);
    expect(ev.defaultPrevented).toBe(true);
  });

  it.each([
    "Shift", "Control", "Alt", "AltGraph", "Meta", "CapsLock", "NumLock", "ScrollLock", "Fn",
  ])("preserves ]q across a %s press and release", async (modifier) => {
    const run = await start();
    key("]");
    expect(key(modifier).defaultPrevented).toBe(false);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).toContain("]");
    window.dispatchEvent(new KeyboardEvent("keyup", { key: modifier }));
    const ev = key("q");
    expect(run).toHaveBeenCalledWith("item.next", ev);
    expect(ev.defaultPrevented).toBe(true);
  });

  it.each(["Super", "Hyper", "OS"])("cancels pending keys and their fallback on %s", async (modifier) => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    const run = await start();
    key("g");
    expect(key(modifier).defaultPrevented).toBe(false);
    window.dispatchEvent(new KeyboardEvent("keyup", { key: modifier }));
    vi.advanceTimersByTime(1001);
    expect(run).not.toHaveBeenCalled();
    expect(document.querySelector("#eitri-keys-hud")!.textContent).not.toContain("g");
  });

  it("does not extend a pending prefix's deadline for a modifier press", async () => {
    const run = await start();
    key("]");
    vi.advanceTimersByTime(900);
    key("Shift", { shiftKey: true });
    expect(document.querySelector("#eitri-keys-hud")!.textContent).toContain("]");
    vi.advanceTimersByTime(101);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).not.toContain("]");
    key("q");
    expect(run).not.toHaveBeenCalled();
  });

  it("clears the displayed prefix when its timer expires", async () => {
    await start();
    key("]");
    expect(document.querySelector("#eitri-keys-hud")!.textContent).toContain("]");
    vi.advanceTimersByTime(1001);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).not.toContain("]");
  });

  it("starts from the page's initial focus and timeout", async () => {
    const run = await start(vi.fn(() => true), { timeoutlen: 300, hasKeys: false });
    expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.hidden).toBe(true);
    events.get("focus")!({ has_keys: true });
    key("]");
    vi.advanceTimersByTime(301);
    key("q");
    expect(run).not.toHaveBeenCalled();
  });

  it.each(["input", "textarea", "select", "editable"])("leaves keys in a focused %s and Esc blurs it", async (tag) => {
    const run = await start();
    const field = document.createElement(tag === "editable" ? "div" : tag);
    if (tag === "editable") { field.setAttribute("contenteditable", "true"); field.tabIndex = 0; }
    document.body.append(field);
    field.focus();
    expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.dataset.mode).toBe("insert");
    for (const part of ["j", "f", "1", "?", "Enter", " "]) {
      expect(key(part, {}, field).defaultPrevented).toBe(false);
    }
    expect(document.activeElement).toBe(field);
    expect(run).not.toHaveBeenCalled();
    expect(window.scrollBy).not.toHaveBeenCalled();
    expect(document.querySelector("#eitri-keys-sheet")).toBeNull();
    expect(key("Escape", {}, field).defaultPrevented).toBe(true);
    expect(document.activeElement).not.toBe(field);
    expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.dataset.mode).toBe("normal");
  });

  it.each([{ isComposing: true }, { keyCode: 229 }])("does not blur an input for composing Escape %j", async (init) => {
    await start();
    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    expect(key("Escape", init, input).defaultPrevented).toBe(false);
    expect(document.activeElement).toBe(input);
  });

  it("fires a key that starts a longer one after timeoutlen, and still completes the longer one", async () => {
    effective.find((row) => row.action === "item.next")!.keys.push("g");
    const scrollTo = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    const run = await start();
    expect(key("g").defaultPrevented).toBe(true);
    key("g");
    expect(scrollTo).toHaveBeenCalledTimes(1);
    expect(run).not.toHaveBeenCalled();
    const ev = key("g");
    vi.advanceTimersByTime(999);
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(2);
    expect(run).toHaveBeenCalledWith("item.next", ev);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).not.toContain("g");
  });

  it.each(["timer", "mismatch"])("keeps g's fallback and event after gx on %s", async (ending) => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    effective.push({ action: "item.long", keys: ["gxy"], context: "page", desc: "long" });
    effective.push({ action: "item.other", keys: ["q"], context: "page", desc: "other" });
    const run = await start();
    const first = key("g");
    key("x");
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else {
      const last = key("q");
      expect(run).toHaveBeenNthCalledWith(2, "item.other", last);
    }
    expect(run).toHaveBeenNthCalledWith(1, "item.next", first);
    expect(run).toHaveBeenCalledTimes(ending === "timer" ? 1 : 2);
  });

  it.each(["timer", "mismatch", "late key"])("replays buffered keys after the fallback on %s", async (ending) => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    effective.push({ action: "item.long", keys: ["gxyz"], context: "page", desc: "long" });
    for (const part of ["x", "y", "q"]) {
      effective.push({ action: `item.${part}`, keys: [part], context: "page", desc: part });
    }
    const run = await start();
    const clock = vi.spyOn(performance, "now").mockReturnValue(0);
    const first = key("g");
    const x = key("x");
    const y = key("y");
    const expected = [["item.next", first], ["item.x", x], ["item.y", y]];
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else {
      if (ending === "late key") clock.mockReturnValue(1001);
      expected.push(["item.q", key("q")]);
    }
    expect(run.mock.calls).toEqual(expected);
    expected.forEach(([, event], i) => expect(run.mock.calls[i][1]).toBe(event));
  });

  it("lets a buffered key and the ending key complete a fresh sequence", async () => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    effective.push({ action: "item.long", keys: ["gxy"], context: "page", desc: "long" });
    effective.push({ action: "item.other", keys: ["xq"], context: "page", desc: "other" });
    const run = await start();
    const first = key("g");
    key("x");
    const last = key("q");
    expect(run.mock.calls).toEqual([["item.next", first], ["item.other", last]]);
    expect(run.mock.calls[0][1]).toBe(first);
    expect(run.mock.calls[1][1]).toBe(last);
  });

  it.each(["timer", "mismatch"])("uses the event completing a multi-key fallback on %s", async (ending) => {
    effective.find((row) => row.action === "item.pick")!.keys = ["<Space>1"];
    effective.push({ action: "item.long", keys: ["<Space>1x"], context: "page", desc: "long" });
    const run = await start();
    key(" ");
    const completed = key("1");
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else expect(key("Tab").defaultPrevented).toBe(false);
    expect(run).toHaveBeenCalledTimes(1);
    expect(run.mock.calls[0][0]).toBe("item.pick");
    expect(run.mock.calls[0][1]).toBe(completed);
  });

  it.each(["timer", "mismatch"])("updates the fallback event when a longer binding completes on %s", async (ending) => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    effective.find((row) => row.action === "scroll.top")!.keys = ["<Home>"];
    effective.find((row) => row.action === "item.pick")!.keys = ["gg"];
    effective.push({ action: "item.long", keys: ["ggx"], context: "page", desc: "long" });
    const run = await start();
    key("g");
    const completed = key("g");
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else key("q");
    expect(run).toHaveBeenCalledTimes(1);
    expect(run.mock.calls[0][0]).toBe("item.pick");
    expect(run.mock.calls[0][1]).toBe(completed);
  });

  it.each(["timer", "mismatch"])("uses Esc's event when it cancels and restarts a wait on %s", async (ending) => {
    effective.find((row) => row.action === "mode.escape")!.keys = [];
    effective.push({ action: "item.escape", keys: ["<Esc>"], context: "page", desc: "escape" });
    effective.push({ action: "item.long", keys: ["<Esc>x"], context: "page", desc: "long" });
    const run = await start();
    key("g");
    const escape = key("Escape");
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else key("q");
    expect(run).toHaveBeenCalledTimes(1);
    expect(run.mock.calls[0][0]).toBe("item.escape");
    expect(run.mock.calls[0][1]).toBe(escape);
  });

  it("fires g before replaying j as scroll.down", async () => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    const order: string[] = [];
    const run = await start(vi.fn(() => { order.push("item.next"); return true; }));
    vi.mocked(window.scrollBy).mockImplementation(() => { order.push("scroll.down"); });
    const first = key("g");
    expect(key("j").defaultPrevented).toBe(true);
    expect(order).toEqual(["item.next", "scroll.down"]);
    expect(run).toHaveBeenCalledWith("item.next", first);
  });

  it("passes the 1 event to item.pick and replays q with its own event", async () => {
    effective.push({ action: "item.long", keys: ["1x"], context: "page", desc: "long" });
    effective.push({ action: "item.other", keys: ["q"], context: "page", desc: "other" });
    const run = await start();
    const first = key("1");
    const last = key("q");
    expect(run.mock.calls).toEqual([["item.pick", first], ["item.other", last]]);
  });

  it("does not prevent Tab when it ends g's wait", async () => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    const run = await start();
    const first = key("g");
    expect(key("Tab").defaultPrevented).toBe(false);
    expect(run).toHaveBeenCalledWith("item.next", first);
  });

  it("runs a declared sequence that ends in Esc", async () => {
    effective.push({ action: "item.custom", keys: ["g<Esc>"], context: "阅读", desc: "自定" });
    const run = await start();
    key("g");
    const ev = key("Escape");
    expect(run).toHaveBeenCalledWith("item.custom", ev);
  });

  it("gives a page row the generic key it also names", async () => {
    const find: Row = { action: "item.find", keys: ["f"], context: "页面", desc: "查找" };
    call.mockImplementation(async (_verb: string, args: { rows: Row[] }) => ({ rows: args.rows }));
    const run = vi.fn(() => true);
    cleanup = await install({ rows: [find], run });
    const declared: Row[] = call.mock.calls[0][1].rows;
    expect(declared.map((row) => row.action)).not.toContain("hint.start");
    expect(declared.filter((row) => row.keys.includes("f"))).toEqual([find]);
    const ev = key("f");
    expect(run).toHaveBeenCalledWith("item.find", ev);
    expect(document.querySelector("#eitri-keys-hints")).toBeNull();
  });

  it("keeps a generic row in place when a page redeclares its action", async () => {
    const hint: Row = { action: "hint.start", keys: ["F"], context: "页面", desc: "提示" };
    const find: Row = { action: "item.find", keys: ["f"], context: "页面", desc: "查找" };
    cleanup = await install({ rows: [find, hint], run: vi.fn(() => true) });
    const declared: Row[] = call.mock.calls[0][1].rows;
    expect(declared.map((row) => row.action)).toEqual([...GENERIC_ROWS.map((row) => row.action), "item.find"]);
    expect(declared.find((row) => row.action === "hint.start")).toEqual(hint);
  });

  it.each(["checkbox", "radio", "button", "range", "color", "file"])("keeps the page's keys on a focused %s input",
    async (type) => {
      await start();
      const input = document.createElement("input");
      input.type = type;
      document.body.append(input);
      input.focus();
      expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.dataset.mode).toBe("normal");
      expect(key("j", {}, input).defaultPrevented).toBe(true);
      expect(window.scrollBy).toHaveBeenCalledTimes(1);
    });

  it.each([true, false])("run returning %s controls default prevention", async (handled) => {
    const run = await start(vi.fn(() => handled));
    const ev = key("1");
    expect(run).toHaveBeenCalledWith("item.pick", ev);
    expect(ev.defaultPrevented).toBe(handled);
  });

  it("lets document handlers consume an event before the bubble listener", async () => {
    const run = await start();
    document.addEventListener("keydown", (ev) => ev.preventDefault(), { once: true });
    key("1");
    expect(run).not.toHaveBeenCalled();
  });

  it.each([
    ["j", { ctrlKey: true }], ["n", { ctrlKey: true, shiftKey: true }],
    ["n", { altKey: true }], ["n", { metaKey: true }],
  ])("never acts on reserved or unrepresentable modifiers: %s %j", async (part, init) => {
    effective[0].keys.push("<C-j>", "<C-S-n>", "<C-n>");
    await start();
    expect(key(part as string, init as KeyboardEventInit).defaultPrevented).toBe(false);
    expect(window.scrollBy).not.toHaveBeenCalled();
  });

  it("tracks both physical Super keys and resets after blur", async () => {
    await start();
    key("Super", { code: "OSLeft" });
    key("Super", { code: "OSRight" });
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Super", code: "OSRight" }));
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
    window.dispatchEvent(new Event("blur"));
    key("j");
    expect(window.scrollBy).toHaveBeenCalledTimes(1);
  });

  it("keeps Super held when a focus event reaffirms ownership", async () => {
    await start();
    key("Super", { code: "OSLeft" });
    events.get("focus")!({ has_keys: true });
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
  });

  it("renders effective rows as text in the sheet and closes on Escape", async () => {
    effective.push({ action: "test.text", keys: ["x"], context: "<b>阅读</b>", desc: '<img src=x onerror="alert(1)">' });
    effective.find((row) => row.action === "scroll.down")!.keys = ["<C-n>"];
    await start();
    key("?", { shiftKey: true });
    const sheet = document.querySelector("#eitri-keys-sheet")!;
    expect(sheet.textContent).toContain("<C-n>");
    expect(sheet.textContent).toContain('<img src=x onerror="alert(1)">');
    expect(sheet.querySelector("img, b")).toBeNull();
    key("Escape");
    expect(document.querySelector("#eitri-keys-sheet")).toBeNull();
  });

  it("reinstall replaces listeners instead of running actions twice", async () => {
    await start();
    await start();
    key("j");
    expect(window.scrollBy).toHaveBeenCalledTimes(1);
    expect(document.querySelectorAll("#eitri-keys-hud")).toHaveLength(1);
  });

  it("does not bind undeclared defaults if declaration fails", async () => {
    call.mockRejectedValueOnce(new Error("refused"));
    await expect(start()).rejects.toThrow("refused");
    key("j");
    expect(window.scrollBy).not.toHaveBeenCalled();
    expect(events.size).toBe(0);
  });
});


describe("status() and keys()", () => {
  it("clears the status on a new installation and on removal", async () => {
    await start();
    window.eitriKeys.status("item 1/8");
    expect(document.querySelector(".eitri-keys-status")!.textContent).toBe("item 1/8");
    await start();
    expect(document.querySelector(".eitri-keys-status")!.textContent).toBe("");
    window.eitriKeys.status("item 2/8");
    cleanup!();
    cleanup = undefined;
    expect(document.getElementById("eitri-keys-hud")).toBeNull();
    window.eitriKeys.message("x");
    expect(document.querySelector(".eitri-keys-status")!.textContent).toBe("");
  });

  it("is empty before the installation resolves, then gives the reply's keys", async () => {
    let answer!: (value: unknown) => void;
    call.mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
    const pending = install({ rows: pageRows, run: vi.fn(() => true) });
    expect(window.eitriKeys.keys("item.next")).toEqual([]);
    effective.find((row) => row.action === "item.next")!.keys = ["<C-n>"];
    answer({ rows: effective });
    cleanup = await pending;
    expect(window.eitriKeys.keys("item.next")).toEqual(["<C-n>"]);
    expect(window.eitriKeys.keys("hint.start")).toEqual(["f"]);
    expect(window.eitriKeys.keys("item.unknown")).toEqual([]);
  });

  it("reads the host's key/source pairs", async () => {
    call.mockResolvedValueOnce({ rows: effective.map((row) => ({
      ...row, keys: row.action === "item.next" ? [["]x", "init.lua"], ["]q", "default"]] : row.keys.map((k) => [k, "default"]),
    })) });
    await start();
    expect(window.eitriKeys.keys("item.next")).toEqual(["]x", "]q"]);
  });

  it("gives an empty list for an action whose keys the host removed", async () => {
    effective.find((row) => row.action === "scroll.down")!.keys = [];
    await start();
    expect(window.eitriKeys.keys("scroll.down")).toEqual([]);
  });

  it("returns a copy each call", async () => {
    await start();
    const first = window.eitriKeys.keys("item.next");
    first.push("x");
    expect(window.eitriKeys.keys("item.next")).toEqual(["]q"]);
  });

  it("is empty after the installation is removed", async () => {
    await start();
    cleanup!();
    cleanup = undefined;
    expect(window.eitriKeys.keys("item.next")).toEqual([]);
  });

  it("keeps keys() on the newest installation when an older one resolves late", async () => {
    let answerOld!: (value: unknown) => void;
    call.mockImplementationOnce(() => new Promise((resolve) => { answerOld = resolve; }));
    const old = install({ rows: pageRows, run: vi.fn(() => true) });
    cleanup = await install({ rows: pageRows, run: vi.fn(() => true) });
    answerOld({ rows: effective.map((row) => row.action === "item.next" ? { ...row, keys: ["<C-x>"] } : row) });
    await old;
    expect(window.eitriKeys.keys("item.next")).toEqual(["]q"]);
  });
});
