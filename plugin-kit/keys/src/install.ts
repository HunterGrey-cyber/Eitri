import { effectiveRows, hasKeys, pluginBridge, timeoutOf } from "./bridge";
import { blockedEvent, shouldSkip, textField } from "./events";
import { installHeldSuperTracking, reset as resetSuper, superHeld } from "./held-super";
import { activate, startHints } from "./hints";
import { setDimmed, showMessage, showMode } from "./hud";
import { strokeOf } from "./notation";
import { createResolver } from "./resolver";
import { GENERIC_ROWS } from "./rows";
import { runScroll } from "./scroll";
import { closeSheet, openSheet, sheetIsOpen } from "./sheet";
import type { InstallOptions } from "./index";

export type { HintTarget, InstallOptions } from "./index";

let current: Element | null = null;
let pageHasKeys = true;
let removeInstallation: (() => void) | undefined;
let generation = 0;

const MODIFIER_KEYS = new Set([
  "Shift", "Control", "Alt", "AltGraph", "Meta", "Super", "Hyper", "CapsLock", "NumLock", "ScrollLock", "Fn", "OS",
]);

export function setCurrent(el: Element | null): void {
  current?.classList.remove("eitri-keys-current", "eitri-keys-dimmed");
  current = el;
  current?.classList.add("eitri-keys-current");
  current?.classList.toggle("eitri-keys-dimmed", !pageHasKeys);
}

export async function install(opts: InstallOptions): Promise<() => void> {
  removeInstallation?.();
  const ticket = ++generation;
  const bridge = pluginBridge();
  const declared = new Map(GENERIC_ROWS.map((row) => [row.action, row]));
  for (const row of opts.rows) declared.set(row.action, row);
  const updates: Array<() => void> = [];
  let ready = false;
  let hints: ReturnType<typeof startHints> | undefined;
  const receive = (update: () => void) => {
    if (ready) update();
    else updates.push(update);
  };
  // Host updates can arrive during declaration; replay them after the initial state and bindings exist.
  const focus = bridge.on("focus", (data) => receive(() => {
    const value = hasKeys(data);
    if (value === undefined) return;
    pageHasKeys = value;
    if (!value) hints?.cancel();
    clearPrefix();
    if (!value) resetSuper();
    setDimmed(!value);
    current?.classList.toggle("eitri-keys-dimmed", !value);
  }));
  const config = bridge.on("keys_config", (data) => receive(() => {
    const value = timeoutOf(data);
    if (value !== undefined) { timeoutlen = value; clearPrefix(); }
  }));
  let rows: ReturnType<typeof effectiveRows>;
  try {
    rows = effectiveRows(await bridge.call("keys.declare", { rows: [...declared.values()] }));
  } catch (error) {
    focus();
    config();
    throw error;
  }
  if (ticket !== generation) {
    focus();
    config();
    return () => {};
  }

  let timeoutlen = opts.initial?.timeoutlen ?? 1000;
  pageHasKeys = opts.initial?.hasKeys ?? true;
  let pending = "";
  let timer: number | undefined;
  const resolver = createResolver(rows, () => timeoutlen, () => performance.now());
  const renderMode = () => showMode(textField(document.activeElement) ? "insert" : hints?.active ? "hint" : "normal",
    hints?.active ? hints.prefix : pending);
  const clearPrefix = () => {
    window.clearTimeout(timer);
    timer = undefined;
    pending = "";
    resolver.reset();
    renderMode();
  };
  ready = true;
  for (const update of updates) update();
  updates.length = 0;
  const stopSuper = installHeldSuperTracking(document, window);
  const syncFocus = () => { hints?.cancel(); clearPrefix(); };
  document.addEventListener("focusin", syncFocus);
  document.addEventListener("focusout", syncFocus);
  const onBlur = () => { hints?.cancel(); clearPrefix(); };
  window.addEventListener("blur", onBlur);
  const onViewportChange = () => {
    if (!hints?.active) return;
    hints.cancel();
    clearPrefix();
  };
  // Nested scrollers move targets too, and their scroll events do not bubble.
  window.addEventListener("scroll", onViewportChange, true);
  window.addEventListener("resize", onViewportChange);

  const onKey = (ev: KeyboardEvent) => {
    if (!pageHasKeys) return;
    if (MODIFIER_KEYS.has(ev.key)) return;
    const held = superHeld();
    if (shouldSkip(ev, held)) {
      // Only the resolver may authorize a Ctrl chord; the shell's navigation and IME chords stay reserved.
      if (blockedEvent(ev, held) || !ev.ctrlKey || ev.altKey || ev.metaKey || ev.shiftKey ||
        /^[hjkl]$/i.test(ev.key)) return;
    }
    const field = textField(document.activeElement);
    if (field) {
      if (ev.key === "Escape" && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
        field.blur();
        clearPrefix();
        ev.preventDefault();
      }
      return;
    }
    if (hints?.active) {
      if (ev.ctrlKey || ev.altKey || ev.metaKey) return;
      const target = hints.feed(ev.key);
      if (target) activate(target);
      renderMode();
      ev.preventDefault();
      return;
    }
    if (sheetIsOpen()) {
      if (ev.key === "Escape") { closeSheet(); clearPrefix(); ev.preventDefault(); }
      return;
    }
    if (ev.key === "Escape") clearPrefix();
    const result = resolver.feed(strokeOf(ev));
    window.clearTimeout(timer);
    timer = undefined;
    pending = "";
    if (result.kind === "pending") {
      pending = result.prefix;
      timer = window.setTimeout(clearPrefix, timeoutlen + 1);
      renderMode();
      ev.preventDefault();
      return;
    }
    renderMode();
    if (result.kind !== "action") return;
    let handled: boolean;
    switch (result.action) {
      case "sheet.open": openSheet(rows); handled = true; break;
      case "mode.escape": handled = true; break;
      case "hint.start": {
        clearPrefix();
        const rect = current?.getBoundingClientRect();
        const origin = rect ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } :
          { x: window.innerWidth / 2, y: window.innerHeight / 2 };
        hints = startHints(document, opts.targets?.() ?? [], origin);
        renderMode();
        handled = true;
        break;
      }
      default:
        handled = GENERIC_ROWS.some((row) => row.action === result.action) ?
          runScroll(result.action) : opts.run(result.action, ev);
    }
    if (handled) ev.preventDefault();
  };
  window.addEventListener("keydown", onKey);
  showMessage("");
  setDimmed(!pageHasKeys);
  current?.classList.toggle("eitri-keys-dimmed", !pageHasKeys);
  renderMode();

  let removed = false;
  const remove = () => {
    if (removed) return;
    removed = true;
    hints?.cancel();
    window.clearTimeout(timer);
    window.removeEventListener("keydown", onKey);
    window.removeEventListener("blur", onBlur);
    window.removeEventListener("scroll", onViewportChange, true);
    window.removeEventListener("resize", onViewportChange);
    document.removeEventListener("focusin", syncFocus);
    document.removeEventListener("focusout", syncFocus);
    stopSuper();
    focus();
    config();
    closeSheet();
    setCurrent(null);
    document.getElementById("eitri-keys-hud")?.remove();
    if (removeInstallation === remove) removeInstallation = undefined;
  };
  removeInstallation = remove;
  return remove;
}
