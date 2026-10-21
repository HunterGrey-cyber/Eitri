// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { install, setCurrent } from "./install";
import { GENERIC_ROWS, type Row } from "./rows";
import type { InstallOptions } from "./index";
import "./index";

const pageRows: Row[] = [
  { action: "item.next", keys: ["]q"], context: "阅读", desc: "下一题" },
  { action: "item.pick", keys: ["1"], context: "题目", desc: "选择" },
];
let events: Map<string, (data: unknown) => void>;
let effective: Row[];
let call: ReturnType<typeof vi.fn>;
let cleanup: (() => void) | undefined;
let y = 0;
let reduceMotion = true;

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
  // With reduced motion every scroll action is one instant write; the glide's motion is tested in glide.test.ts.
  y = 0;
  reduceMotion = true;
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => y });
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string) => ({ matches: reduceMotion && query.includes("reduce") }),
  });
  vi.spyOn(window, "scrollTo").mockImplementation(((opts: ScrollToOptions) => { y = opts.top!; }) as typeof window.scrollTo);
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  document.body.replaceChildren();
  delete (window as unknown as Record<string, unknown>).matchMedia;
  vi.restoreAllMocks();
  vi.useRealTimers();
});

/** A prefix with no shorter binding waits for its next key by default; the timeout cases turn that off. */
const timing: InstallOptions["initial"] = { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b", settings: { prefix_wait: false } };

async function start(run = vi.fn(() => true), initial: InstallOptions["initial"] = { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" }) {
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
    expect(window.eitriKeys).toMatchObject({ version: 3, install, setCurrent });
    for (const name of ["message", "status", "keys", "findRefresh", "findSeed"] as const) {
      expect(typeof window.eitriKeys[name]).toBe("function");
    }
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
    expect(window.scrollTo).toHaveBeenCalledWith({ top: 60, behavior: "instant" });
  });

  it("reads the host's key/source pairs in effective rows", async () => {
    call.mockResolvedValueOnce({ rows: effective.map((row) => ({
      ...row, keys: row.action === "scroll.down" ? [["<C-n>", "init.lua"]] : row.keys.map((key) => [key, "default"]),
    })) });
    await start();
    key("n", { ctrlKey: true });
    expect(window.scrollTo).toHaveBeenCalledWith({ top: 60, behavior: "instant" });
  });

  it("does not retain a binding removed by the host", async () => {
    effective.find((row) => row.action === "scroll.down")!.keys = [];
    await start();
    expect(key("j").defaultPrevented).toBe(false);
    expect(window.scrollTo).not.toHaveBeenCalled();
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
    expect(window.scrollTo).not.toHaveBeenCalled();
    events.get("focus")!({ has_keys: true });
    expect(hud.hidden).toBe(false);
    expect(current.classList.contains("eitri-keys-dimmed")).toBe(false);
  });

  it("applies keys_config timeout changes to later prefixes", async () => {
    const run = await start(vi.fn(() => true), timing);
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
    expect(window.scrollTo).not.toHaveBeenCalled();
    reply({ rows: effective });
    await starting;
    expect(document.querySelector<HTMLElement>("#eitri-keys-hud")!.hidden).toBe(true);
    key("j");
    expect(window.scrollTo).not.toHaveBeenCalled();
    events.get("focus")!({ has_keys: true });
    key("j");
    expect(window.scrollTo).toHaveBeenCalledTimes(1);
  });

  it("uses configuration received during declaration with the effective bindings", async () => {
    let reply!: (value: unknown) => void;
    call.mockImplementationOnce(() => new Promise((resolve) => { reply = resolve; }));
    const run = vi.fn(() => true);
    const starting = start(run, timing);
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
    const run = await start(vi.fn(() => true), timing);
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
    await start(vi.fn(() => true), timing);
    key("]");
    expect(document.querySelector("#eitri-keys-hud")!.textContent).toContain("]");
    vi.advanceTimersByTime(1001);
    expect(document.querySelector("#eitri-keys-hud")!.textContent).not.toContain("]");
  });

  it("starts from the page's initial focus and timeout", async () => {
    const run = await start(vi.fn(() => true), { timeoutlen: 300, hasKeys: false, settings: { prefix_wait: false } });
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
    expect(window.scrollTo).not.toHaveBeenCalled();
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
    y = 500;
    const run = await start();
    expect(key("g").defaultPrevented).toBe(true);
    key("g");
    expect(window.scrollTo).toHaveBeenCalledTimes(1);
    expect(window.scrollTo).toHaveBeenCalledWith({ top: 0, behavior: "instant" });
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

  it.each(["timer", "mismatch"])("Esc ends a wait without starting its own on %s", async (ending) => {
    effective.find((row) => row.action === "mode.escape")!.keys = [];
    effective.push({ action: "item.escape", keys: ["<Esc>"], context: "page", desc: "escape" });
    effective.push({ action: "item.long", keys: ["<Esc>x"], context: "page", desc: "long" });
    const run = await start();
    key("g");
    expect(key("Escape").defaultPrevented).toBe(true);
    expect(document.querySelector(".eitri-keys-pending")!.textContent).toBe("");
    if (ending === "timer") vi.advanceTimersByTime(1001);
    else key("x");
    expect(run).not.toHaveBeenCalled();
  });

  it("fires g before replaying j as scroll.down", async () => {
    effective.find((row) => row.action === "item.next")!.keys = ["g"];
    const order: string[] = [];
    const run = await start(vi.fn(() => { order.push("item.next"); return true; }));
    vi.mocked(window.scrollTo).mockImplementation(() => { order.push("scroll.down"); });
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
    cleanup = await install({ rows: [find, hint], run: vi.fn(() => true),
      initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" } });
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
      expect(window.scrollTo).toHaveBeenCalledTimes(1);
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
    expect(window.scrollTo).not.toHaveBeenCalled();
  });

  it("tracks both physical Super keys and resets after blur", async () => {
    await start();
    key("Super", { code: "OSLeft" });
    key("Super", { code: "OSRight" });
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Super", code: "OSRight" }));
    key("j");
    expect(window.scrollTo).not.toHaveBeenCalled();
    window.dispatchEvent(new Event("blur"));
    key("j");
    expect(window.scrollTo).toHaveBeenCalledTimes(1);
  });

  it("keeps Super held when a focus event reaffirms ownership", async () => {
    await start();
    key("Super", { code: "OSLeft" });
    events.get("focus")!({ has_keys: true });
    key("j");
    expect(window.scrollTo).not.toHaveBeenCalled();
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
    expect(window.scrollTo).toHaveBeenCalledTimes(1);
    expect(document.querySelectorAll("#eitri-keys-hud")).toHaveLength(1);
  });

  it("does not bind undeclared defaults if declaration fails", async () => {
    call.mockRejectedValueOnce(new Error("refused"));
    await expect(start()).rejects.toThrow("refused");
    key("j");
    expect(window.scrollTo).not.toHaveBeenCalled();
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

describe("the window's prefix and the settings", () => {
  const declared = () => (call.mock.calls[0][1] as { rows: Row[] }).rows;
  it("keeps every generic Ctrl default under the stock prefix", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" });
    expect(declared()).toEqual([...GENERIC_ROWS, ...pageRows]);
  });
  it("drops a generic Ctrl default equal to the window's prefix before declaring", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+d" });
    expect(declared().find((row) => row.action === "scroll.half_down")!.keys).toEqual(["d"]);
    expect(declared().find((row) => row.action === "scroll.half_up")!.keys).toEqual(["u", "<C-u>"]);
  });
  it("a page that gives no prefix declares no generic Ctrl key", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true });
    expect(declared().flatMap((row) => row.keys).filter((key) => key.startsWith("<C-"))).toEqual([]);
    expect(declared().find((row) => row.action === "scroll.down")!.keys).toEqual(["j"]);
    expect(declared().some((row) => row.action === "scroll.page_down")).toBe(false);
    cleanup?.();
    call.mockClear();
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, prefix: null });
    expect(declared().flatMap((row) => row.keys).filter((key) => key.startsWith("<C-"))).toEqual([]);
  });
  it("drops a generic row whose only key is the prefix", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+f" });
    expect(declared().some((row) => row.action === "scroll.page_down")).toBe(false);
  });
  it("scrolls by the configured step, on j and on <C-e>", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, settings: { scroll_step: 40 } });
    key("j");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 40, behavior: "instant" });
    key("y", { ctrlKey: true });
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 0, behavior: "instant" });
  });
  it("jumps instead of easing when smooth is off", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, settings: { smooth: false } });
    key("d", { ctrlKey: true });
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: window.innerHeight / 2, behavior: "instant" });
  });
  it("pages down by the view less two steps on <C-f>", async () => {
    Object.defineProperty(window, "innerHeight", { configurable: true, value: 800 });
    await start();
    key("f", { ctrlKey: true });
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 680, behavior: "instant" });
  });
  it("goes back and forward on <C-o> and <C-i>", async () => {
    const back = vi.spyOn(window.history, "back").mockImplementation(() => {});
    const forward = vi.spyOn(window.history, "forward").mockImplementation(() => {});
    await start();
    expect(key("o", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(key("i", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(back).toHaveBeenCalledOnce();
    expect(forward).toHaveBeenCalledOnce();
  });
  it("a later installation without settings goes back to the defaults", async () => {
    await start(vi.fn(() => true), { timeoutlen: 1000, hasKeys: true, settings: { scroll_step: 40 } });
    cleanup?.();
    call.mockClear();
    await start();
    key("j");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 60, behavior: "instant" });
  });
});

describe("prefixes that wait, Space and Esc", () => {
  const part = (name: string) => document.querySelector<HTMLElement>(`.eitri-keys-${name}`)!;
  const folds: Row[] = [
    { action: "fold.toggle", keys: ["za"], context: "阅读", desc: "折叠" },
    { action: "fold.open_all", keys: ["zR"], context: "阅读", desc: "全展开" },
    { action: "fold.close_all", keys: ["zM"], context: "阅读", desc: "全折叠" },
    { action: "code.left", keys: ["zh"], context: "阅读", desc: "代码左" },
  ];
  const spaceRow: Row = { action: "toc.toggle", keys: ["<Space>e"], context: "阅读", desc: "目录" };
  const escapes = [["Escape", {}], ["[", { ctrlKey: true }]] as const;

  function focused<T extends HTMLElement>(el: T, parent: Element = document.body): T {
    parent.append(el);
    el.focus();
    return el;
  }
  function button(parent?: Element) {
    return focused(document.createElement("button"), parent);
  }
  function link(parent?: Element) {
    const a = document.createElement("a");
    a.href = "#elsewhere";
    return focused(a, parent);
  }

  it("a pending z waits with no timeout and shows the which-key line after the delay", async () => {
    effective.push(...folds);
    const run = await start(vi.fn(() => true),
      { timeoutlen: 300, hasKeys: true, prefix: "Ctrl+b", settings: { which_key_delay: 200 } });
    window.eitriKeys.message("saved");
    key("z");
    vi.advanceTimersByTime(199);
    expect(part("whichkey").textContent).toBe("");
    expect(part("message").hidden).toBe(false);
    vi.advanceTimersByTime(1);
    expect(part("whichkey").textContent).toBe("z → a 折叠 · R 全展开 · M 全折叠 · h 代码左");
    expect(part("message").hidden).toBe(true);
    vi.advanceTimersByTime(10_000);
    expect(part("pending").textContent).toBe("z");
    const ev = key("a");
    expect(run).toHaveBeenCalledWith("fold.toggle", ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(part("whichkey").textContent).toBe("");
    expect(part("message").hidden).toBe(false);
    expect(part("message").textContent).toBe("saved");
  });

  it("which_key_delay false never shows the line", async () => {
    effective.push(...folds);
    const run = await start(vi.fn(() => true),
      { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b", settings: { which_key_delay: null } });
    key("z");
    vi.advanceTimersByTime(60_000);
    expect(part("whichkey").textContent).toBe("");
    expect(part("pending").textContent).toBe("z");
    key("R", { shiftKey: true });
    expect(run).toHaveBeenCalledWith("fold.open_all", expect.any(KeyboardEvent));
  });

  it.each(escapes)("Esc after a pending prefix only cancels it (%s)", async (name, init) => {
    effective.push(...folds);
    effective.find((row) => row.action === "item.next")!.keys.push("g");
    const run = await start();
    const outside = link();
    for (const prefix of ["z", "g"]) {
      key(prefix, {}, outside);
      vi.advanceTimersByTime(200);
      expect(part("whichkey").textContent).not.toBe("");
      const ev = key(name, init, outside);
      expect(ev.defaultPrevented).toBe(true);
      expect(part("whichkey").textContent).toBe("");
      expect(part("pending").textContent).toBe("");
      vi.advanceTimersByTime(5000);
      expect(run).not.toHaveBeenCalled();
      expect(window.scrollTo).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(outside);
    }
  });

  it("a pending prefix is dropped by a key the kit lets through", async () => {
    effective.push(spaceRow, { action: "toc.open", keys: ["gO"], context: "阅读", desc: "目录" });
    const run = await start();
    const control = button();
    key(" ", {}, control);
    expect(part("pending").textContent).toBe("<Space>");
    expect(key("Enter", {}, control).defaultPrevented).toBe(false);
    expect(part("pending").textContent).toBe("");
    key("e", {}, control);
    expect(run).not.toHaveBeenCalled();
    control.blur();
    key("]");
    document.addEventListener("keydown", (ev) => ev.preventDefault(), { once: true });
    key("x");
    expect(part("pending").textContent).toBe("");
    key("q");
    expect(run).not.toHaveBeenCalled();
    key("g");
    expect(key("Shift", { shiftKey: true }).defaultPrevented).toBe(false);
    expect(part("pending").textContent).toBe("g");
    const ev = key("O", { shiftKey: true });
    expect(run).toHaveBeenCalledWith("toc.open", ev);
  });

  it("with a <Space>e row, Space on an element whose own keydown handler takes Space and stops propagation starts the prefix, and that handler does not run", async () => {
    effective.push(spaceRow);
    const run = await start();
    const line = document.createElement("div");
    line.setAttribute("role", "button");
    line.tabIndex = 0;
    const own = vi.fn();
    line.addEventListener("keydown", (ev) => {
      if (ev.key !== " ") return;
      own();
      ev.preventDefault();
      ev.stopPropagation();
    });
    focused(line);
    const ev = key(" ", {}, line);
    expect(own).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(true);
    expect(part("pending").textContent).toBe("<Space>");
    const e = key("e", {}, line);
    expect(run).toHaveBeenCalledWith("toc.toggle", e);
  });

  it("with a <Space>e row, Space on a focused button goes to the keys: the button is not clicked, nothing scrolls, e runs the row", async () => {
    effective.push(spaceRow);
    const run = await start();
    const control = button();
    const click = vi.fn();
    control.addEventListener("click", click);
    expect(key(" ", {}, control).defaultPrevented).toBe(true);
    const e = key("e", {}, control);
    expect(run).toHaveBeenCalledWith("toc.toggle", e);
    expect(e.defaultPrevented).toBe(true);
    expect(click).not.toHaveBeenCalled();
    expect(window.scrollTo).not.toHaveBeenCalled();
  });

  it("with a <Space>e row, Space then a key that continues nothing does nothing at all", async () => {
    effective.push(spaceRow);
    const run = await start();
    const control = button();
    const click = vi.fn();
    control.addEventListener("click", click);
    expect(key(" ", {}, control).defaultPrevented).toBe(true);
    key("j", {}, control);
    expect(run).not.toHaveBeenCalled();
    expect(click).not.toHaveBeenCalled();
    expect(window.scrollTo).not.toHaveBeenCalled();
    expect(part("pending").textContent).toBe("");
    expect(key(" ").defaultPrevented).toBe(true);
    key("j");
    expect(window.scrollTo).not.toHaveBeenCalled();
  });

  it("with a <Space>e row, Shift+Space keeps its native meaning: an element's own handler gets it, nothing waits", async () => {
    effective.push(spaceRow);
    const run = await start();
    const line = document.createElement("div");
    line.tabIndex = 0;
    const own = vi.fn();
    line.addEventListener("keydown", (ev) => { if (ev.key === " ") own(); });
    focused(line);
    expect(key(" ", { shiftKey: true }, line).defaultPrevented).toBe(false);
    expect(own).toHaveBeenCalledOnce();
    expect(part("pending").textContent).toBe("");
    key("e", {}, line);
    expect(run).not.toHaveBeenCalled();
  });

  it("with a <S-Space>x row, Shift+Space on an element that stops Space starts the prefix", async () => {
    effective.push({ action: "toc.toggle", keys: ["<S-Space>x"], context: "阅读", desc: "目录" });
    const run = await start();
    const line = document.createElement("div");
    line.tabIndex = 0;
    const own = vi.fn();
    line.addEventListener("keydown", (ev) => { if (ev.key === " ") { own(); ev.stopPropagation(); } });
    focused(line);
    expect(key(" ", { shiftKey: true }, line).defaultPrevented).toBe(true);
    expect(own).not.toHaveBeenCalled();
    const x = key("x", {}, line);
    expect(run).toHaveBeenCalledWith("toc.toggle", x);
  });

  it("with a <Space>e row, a key a page handler stops before the kit sees it drops the pending Space", async () => {
    effective.push(spaceRow);
    const run = await start();
    const control = button();
    control.addEventListener("keydown", (ev) => { if (ev.key === "Enter") ev.stopPropagation(); });
    // Settled when the stopped key's dispatch has ended.
    key(" ", {}, control);
    expect(part("pending").textContent).toBe("<Space>");
    key("Enter", {}, control);
    vi.advanceTimersByTime(0);
    expect(part("pending").textContent).toBe("");
    vi.advanceTimersByTime(10_000);
    expect(key("e", {}, control).defaultPrevented).toBe(false);
    expect(run).not.toHaveBeenCalled();
    // Settled by the next key when that one comes first.
    key(" ", {}, control);
    key("Enter", {}, control);
    key("e", {}, control);
    expect(run).not.toHaveBeenCalled();
    // A bare modifier a page stops keeps the prefix, as one that reaches the kit does.
    control.addEventListener("keydown", (ev) => { if (ev.key === "Shift") ev.stopPropagation(); });
    key(" ", {}, control);
    key("Shift", { shiftKey: true }, control);
    vi.advanceTimersByTime(0);
    expect(part("pending").textContent).toBe("<Space>");
    const e = key("e", {}, control);
    expect(run).toHaveBeenCalledWith("toc.toggle", e);
  });

  it("with a <Space>e row, Enter on a focused button still activates it", async () => {
    effective.push(spaceRow);
    const run = await start();
    const control = button();
    expect(key("Enter", {}, control).defaultPrevented).toBe(false);
    expect(run).not.toHaveBeenCalled();
  });

  it("without any <Space> row, Space on a focused button keeps its native meaning and Space on the page is not prevented", async () => {
    const run = await start();
    const control = button();
    const own = vi.fn();
    control.addEventListener("keydown", own);
    expect(key(" ", {}, control).defaultPrevented).toBe(false);
    expect(own).toHaveBeenCalledOnce();
    control.blur();
    expect(key(" ").defaultPrevented).toBe(false);
    expect(part("pending").textContent).toBe("");
    expect(run).not.toHaveBeenCalled();
  });

  it("removing the installation removes the Space listener too", async () => {
    effective.push(spaceRow);
    await start();
    cleanup!();
    cleanup = undefined;
    const control = button();
    const own = vi.fn();
    control.addEventListener("keydown", own);
    expect(key(" ", {}, control).defaultPrevented).toBe(false);
    expect(own).toHaveBeenCalledOnce();
  });

  it("<C-[> leaves insert mode like Esc", async () => {
    await start();
    const input = document.createElement("input");
    input.type = "text";
    focused(input);
    expect(key("[", { ctrlKey: true }, input).defaultPrevented).toBe(true);
    expect(document.activeElement).not.toBe(input);
    expect(part("mode").dataset.mode).toBe("normal");
  });

  it("<C-[> cancels hint mode and closes the sheet like Esc", async () => {
    const target = document.createElement("button");
    document.body.append(target);
    vi.spyOn(target, "getBoundingClientRect").mockReturnValue({
      x: 10, y: 10, left: 10, top: 10, right: 40, bottom: 30, width: 30, height: 20, toJSON: () => ({}),
    });
    vi.spyOn(target, "clientWidth", "get").mockReturnValue(30);
    vi.spyOn(target, "clientHeight", "get").mockReturnValue(20);
    const click = vi.fn();
    target.addEventListener("click", click);
    const run = await start();
    key("f");
    expect(part("mode").dataset.mode).toBe("hint");
    expect(key("[", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(part("mode").dataset.mode).toBe("normal");
    expect(document.querySelectorAll(".eitri-hint")).toHaveLength(0);
    key("?", { shiftKey: true });
    expect(document.querySelector("#eitri-keys-sheet")).not.toBeNull();
    expect(key("[", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(document.querySelector("#eitri-keys-sheet")).toBeNull();
    expect(click).not.toHaveBeenCalled();
    expect(run).not.toHaveBeenCalled();
  });

  it.each(escapes)("Esc in normal mode reaches the page's run after the kit's part (%s)", async (name, init) => {
    const run = await start(vi.fn(() => true));
    const ev = key(name, init);
    expect(run).toHaveBeenCalledOnce();
    expect(run).toHaveBeenCalledWith("mode.escape", ev);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("a page's run on mode.escape sees the focus the key came with", async () => {
    let seen: Element | null = null;
    const run = await start(vi.fn((action: string) => {
      if (action === "mode.escape") seen = document.activeElement;
      return true;
    }));
    const item = document.createElement("div");
    item.tabIndex = -1;
    document.body.append(item);
    setCurrent(item);
    const outside = link();
    key("Escape", {}, outside);
    expect(run).toHaveBeenCalledOnce();
    expect(seen).toBe(outside);
    expect(document.activeElement).toBe(outside);
  });

  it("Esc in normal mode moves focus from a control inside the current item to the item, else blurs it, when the page's run returns false", async () => {
    await start(vi.fn(() => false));
    const item = document.createElement("div");
    item.tabIndex = -1;
    document.body.append(item);
    setCurrent(item);
    const inner = button(item);
    expect(key("Escape", {}, inner).defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(item);
    const outside = link();
    expect(key("[", { ctrlKey: true }, outside).defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(document.body);
  });

  it("a page's run that throws on mode.escape is logged and counts as not taken", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    await start(vi.fn(() => { throw new Error("page failed"); }));
    const outside = link();
    expect(key("Escape", {}, outside).defaultPrevented).toBe(true);
    expect(warn).toHaveBeenCalledOnce();
    expect(document.activeElement).toBe(document.body);
  });
});

describe("a held key's glide", () => {
  let frames: { t: number; phase: string }[] = [];
  beforeEach(() => {
    vi.useRealTimers();
    vi.useFakeTimers({ toFake: ["requestAnimationFrame", "cancelAnimationFrame", "performance", "setTimeout", "clearTimeout"] });
    reduceMotion = false;
    frames = [];
    window.__eitriKeysScrollTrace = (f) => { frames.push(f); };
  });
  afterEach(() => {
    delete window.__eitriKeysScrollTrace;
    delete (document as unknown as Record<string, unknown>).visibilityState;
  });

  /** Presses j and holds it through 300 ms of repeats: the glide is then in its hold. */
  async function holding() {
    await start();
    key("j", { code: "KeyJ" });
    for (let i = 0; i < 9; i++) {
      vi.advanceTimersByTime(33);
      key("j", { code: "KeyJ", repeat: true });
    }
    vi.advanceTimersByTime(16);
    expect(frames.at(-1)!.phase).toBe("hold");
  }

  it.each([
    ["another key", () => { key("x", { code: "KeyX" }); }],
    ["the held key's keyup", () => { window.dispatchEvent(new KeyboardEvent("keyup", { key: "j", code: "KeyJ" })); }],
    ["blur", () => { window.dispatchEvent(new Event("blur")); }],
    ["visibilitychange", () => {
      Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
      document.dispatchEvent(new Event("visibilitychange"));
    }],
    ["focus has_keys false", () => { events.get("focus")!({ has_keys: false }); }],
    ["f", () => { key("f", { code: "KeyF" }); }],
    ["a key a page handler stops", () => {
      const own = document.createElement("div");
      own.addEventListener("keydown", (ev) => ev.stopPropagation());
      document.body.append(own);
      key("x", { code: "KeyX" }, own);
    }],
    ["?", () => { key("?", { code: "Slash", shiftKey: true }); }],
  ] as const)("another key, blur, visibilitychange, focus has_keys false, f and ? each end the hold: %s", async (_name, end) => {
    await holding();
    end();
    const count = frames.length;
    vi.advanceTimersByTime(16);
    expect(frames.length).toBeGreaterThan(count);
    expect(frames.slice(count).every((f) => f.phase === "settle")).toBe(true);
    vi.advanceTimersByTime(400);
    const settled = frames.length;
    vi.advanceTimersByTime(400);
    expect(frames).toHaveLength(settled);
  });

  it("a keyup of another key and the held key's own repeats keep the hold", async () => {
    await holding();
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "x", code: "KeyX" }));
    key("j", { code: "KeyJ", repeat: true });
    vi.advanceTimersByTime(16);
    expect(frames.at(-1)!.phase).toBe("hold");
  });

  it("removing the installation stops the glide at once", async () => {
    await holding();
    const top = y;
    cleanup!();
    cleanup = undefined;
    const count = frames.length;
    vi.advanceTimersByTime(400);
    expect(frames).toHaveLength(count);
    expect(y).toBe(top);
  });
});

describe("the sheet's own keys, through the installation", () => {
  it("q closes the sheet and j scrolls the sheet, not the page", async () => {
    await start();
    key("?", { shiftKey: true });
    const sheet = document.getElementById("eitri-keys-sheet")!;
    Object.defineProperty(sheet, "clientHeight", { configurable: true, value: 300 });
    Object.defineProperty(sheet, "scrollHeight", { configurable: true, value: 2000 });
    expect(key("j", {}, sheet).defaultPrevented).toBe(true);
    expect(sheet.scrollTop).toBe(60);
    expect(window.scrollTo).not.toHaveBeenCalled();
    expect(key("q", {}, sheet).defaultPrevented).toBe(true);
    expect(document.getElementById("eitri-keys-sheet")).toBeNull();
  });
});

describe("a focus list", () => {
  // jsdom lays nothing out, so every element reports no box; a rendered element is one that reports a rect.
  beforeEach(() => {
    vi.spyOn(HTMLElement.prototype, "getClientRects").mockImplementation(function (this: HTMLElement) {
      return (this.dataset.unrendered ? [] : [new DOMRect(0, 0, 10, 10)]) as unknown as DOMRectList;
    });
  });
  it("j and k move focus between the list's links instead of scrolling", async () => {
    document.body.insertAdjacentHTML("beforeend",
      '<nav data-eitri-list><a id="l1" href="#1">1</a><a id="l2" href="#2">2</a><button id="l3">3</button></nav>');
    await start();
    const l1 = document.getElementById("l1")!;
    l1.focus();
    expect(key("j", {}, l1).defaultPrevented).toBe(true);
    expect(document.activeElement!.id).toBe("l2");
    key("j", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("l3");
    key("j", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("l3");
    key("k", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("l2");
    expect(window.scrollTo).not.toHaveBeenCalled();
  });
  it("skips hidden items", async () => {
    document.body.insertAdjacentHTML("beforeend",
      '<nav data-eitri-list><a id="m1" href="#1">1</a><a id="m2" href="#2" hidden>2</a><a id="m3" href="#3">3</a></nav>');
    await start();
    document.getElementById("m1")!.focus();
    key("j", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("m3");
  });
  const folded =
    '<nav data-eitri-list><a id="c1" href="#1">1</a><ol><li><a id="c2" href="#2" data-unrendered="1">2</a></li></ol>' +
    '<a id="c3" href="#3">3</a></nav>';
  it("skips items CSS does not render (a display: none group), by client rects without checkVisibility", async () => {
    document.body.insertAdjacentHTML("beforeend", folded);
    await start();
    document.getElementById("c1")!.focus();
    key("j", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("c3");
    key("k", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("c1");
  });
  it("skips items checkVisibility reports as not rendered", async () => {
    document.body.insertAdjacentHTML("beforeend", folded);
    // The rect check would keep c2; checkVisibility, where the engine has it, decides instead.
    vi.spyOn(HTMLElement.prototype, "getClientRects").mockImplementation(() => [new DOMRect(0, 0, 10, 10)] as unknown as DOMRectList);
    const c2 = document.getElementById("c2")!;
    Object.defineProperty(c2, "checkVisibility", { configurable: true, value: () => false });
    for (const id of ["c1", "c3"]) Object.defineProperty(document.getElementById(id)!, "checkVisibility", { configurable: true, value: () => true });
    await start();
    document.getElementById("c1")!.focus();
    key("j", {}, document.activeElement!);
    expect(document.activeElement!.id).toBe("c3");
  });
});

describe("find rows", () => {
  const declared = () => (call.mock.calls[0][1] as { rows: Row[] }).rows;
  it("the three find rows are declared after link.next", async () => {
    await start();
    const actions = declared().map((row) => row.action);
    const at = actions.indexOf("link.next");
    expect(actions.slice(at, at + 4)).toEqual(["link.next", "find.start", "find.next", "find.prev"]);
    expect(declared().slice(at + 1, at + 4).map((row) => [row.keys.join(" "), row.desc])).toEqual([
      ["/", "find in the page"], ["n", "next match"], ["N", "previous match"],
    ]);
  });
  it("a page that names / itself takes it from find.start", async () => {
    const search: Row = { action: "search", keys: ["/"], context: "book", desc: "search the book" };
    call.mockImplementation(async (_verb: string, args: { rows: Row[] }) => ({ rows: args.rows }));
    const run = vi.fn(() => true);
    cleanup = await install({ rows: [search], run, initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" } });
    expect(declared().map((row) => row.action)).not.toContain("find.start");
    expect(declared().filter((row) => row.keys.includes("/"))).toEqual([search]);
    expect(declared().map((row) => row.action)).toContain("find.next");
    const ev = key("/");
    expect(run).toHaveBeenCalledWith("search", ev);
    expect(document.getElementById("eitri-keys-find")).toBeNull();
  });
});
