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
  it("publishes the versioned API", () => {
    expect(window.eitriKeys).toMatchObject({ version: 1, install, setCurrent });
    expect(typeof window.eitriKeys.message).toBe("function");
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
    "Shift", "Control", "Alt", "AltGraph", "Meta", "Super", "Hyper", "CapsLock", "NumLock", "ScrollLock", "Fn", "OS",
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
