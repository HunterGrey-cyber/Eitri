import { effectiveRows, hasKeys, pluginBridge, timeoutOf } from "./bridge";
import { blockedEvent, shouldSkip, textField } from "./events";
import { installHeldSuperTracking, reset as resetSuper, superHeld } from "./held-super";
import { activate, startHints } from "./hints";
import { forgetStatus, setDimmed, setModeWords, showMessage, showMode, showStatus } from "./hud";
import { canonicalOf, strokeOf } from "./notation";
import { createResolver } from "./resolver";
import { genericRows, isGenericAction, type Row } from "./rows";
import { runScroll } from "./scroll";
import { closeSheet, openSheet, sheetIsOpen } from "./sheet";
import type { InstallOptions, KitLabels } from "./index";

export type { HintTarget, InstallOptions } from "./index";

let current: Element | null = null;
let pageHasKeys = true;
let removeInstallation: (() => void) | undefined;
let generation = 0;
let installedRows: Row[] | null = null;

export function keysOf(action: string): string[] {
  return [...(installedRows?.find((row) => row.action === action)?.keys ?? [])];
}

const MODIFIER_KEYS = new Set([
  "Shift", "Control", "Alt", "AltGraph", "Meta", "Super", "Hyper", "CapsLock", "NumLock", "ScrollLock", "Fn", "OS",
]);

export function setCurrent(el: Element | null): void {
  current?.classList.remove("eitri-keys-current", "eitri-keys-dimmed");
  current = el;
  current?.classList.add("eitri-keys-current");
  current?.classList.toggle("eitri-keys-dimmed", !pageHasKeys);
}

/** The generic rows, then the page's. A page row replaces the generic row of its action in place, and a key a page
 * row holds is taken from the other generic rows (a row left without keys is dropped), so no key names two actions. */
export function mergeRows(page: Row[], labels?: KitLabels): Row[] {
  const spelled = (key: string) => canonicalOf(key) ?? key;
  const taken = new Set(page.flatMap((row) => row.keys.map(spelled)));
  const own = new Set(page.map((row) => row.action));
  const declared = new Map(genericRows(labels).map((row) => [row.action, row]));
  for (const row of page) declared.set(row.action, row);
  return [...declared.values()].flatMap((row) => {
    if (own.has(row.action)) return [row];
    const keys = row.keys.filter((key) => !taken.has(spelled(key)));
    if (!keys.length) return [];
    return [keys.length === row.keys.length ? row : { ...row, keys }];
  });
}

export async function install(opts: InstallOptions): Promise<() => void> {
  removeInstallation?.();
  installedRows = null;
  const ticket = ++generation;
  const bridge = pluginBridge();
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
    rows = effectiveRows(await bridge.call("keys.declare", { rows: mergeRows(opts.rows, opts.labels) }));
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

  installedRows = rows;

  let timeoutlen = opts.initial?.timeoutlen ?? 1000;
  pageHasKeys = opts.initial?.hasKeys ?? true;
  let pending = "";
  let pendingEvents: KeyboardEvent[] = [];
  let timer: number | undefined;
  const resolver = createResolver(rows, () => timeoutlen, () => performance.now());
  const renderMode = () => showMode(textField(document.activeElement) ? "insert" : hints?.active ? "hint" : "normal",
    hints?.active ? hints.prefix : pending);
  const clearPrefix = () => {
    window.clearTimeout(timer);
    timer = undefined;
    pending = "";
    pendingEvents = [];
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
  // Labels follow their targets rather than ending hint mode: a smooth scroll still running, or a list that scrolls
  // itself as output arrives, would otherwise end it the moment it starts. A browser sends these at most once a frame.
  const onViewportChange = () => {
    if (!hints?.active) return;
    hints.reposition();
    renderMode();
  };
  // Nested scrollers move targets too, and their scroll events do not bubble.
  window.addEventListener("scroll", onViewportChange, true);
  window.addEventListener("resize", onViewportChange);

  const runFallback = (action: string, consumed: number) => {
    const completed = pendingEvents[consumed - 1];
    const remaining = pendingEvents.slice(consumed);
    clearPrefix();
    if (perform(action, completed)) completed.preventDefault();
    for (const event of remaining) onKey(event, true);
  };

  const onKey = (ev: KeyboardEvent, replay = false) => {
    if (!pageHasKeys) return;
    const held = superHeld();
    if (held) { clearPrefix(); return; }
    if (MODIFIER_KEYS.has(ev.key)) return;
    // Buffered events already passed the guard; this listener prevented them while they were pending.
    if (!replay && shouldSkip(ev, held)) {
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
    const result = resolver.feed(strokeOf(ev));
    window.clearTimeout(timer);
    timer = undefined;
    pending = "";
    if (result.kind === "fallback") {
      runFallback(result.action, result.consumed);
      onKey(ev, replay);
      return;
    }
    if (result.kind === "pending") {
      pendingEvents.push(ev);
      // Esc may have cancelled the old sequence and begun a new one with only the current event.
      pendingEvents = pendingEvents.slice(-result.length);
      pending = result.prefix;
      timer = window.setTimeout(() => {
        const result = resolver.expire();
        if (result.kind === "fallback" && !superHeld()) runFallback(result.action, result.consumed);
        else clearPrefix();
      }, timeoutlen + 1);
      renderMode();
      ev.preventDefault();
      return;
    }
    pendingEvents = [];
    renderMode();
    if (result.kind !== "action") return;
    if (perform(result.action, ev)) ev.preventDefault();
  };

  /** Runs `action` for the key event `ev` that completed it; true when the key was handled. */
  const perform = (action: string, ev: KeyboardEvent): boolean => {
    let handled: boolean;
    switch (action) {
      case "sheet.open": openSheet(rows, opts.labels); handled = true; break;
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
        handled = isGenericAction(action) ? runScroll(action) : opts.run(action, ev);
    }
    return handled;
  };
  window.addEventListener("keydown", onKey);
  setModeWords(opts.labels?.modes);
  showStatus("");
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
    installedRows = null;
    forgetStatus();
    setModeWords(undefined);
    document.getElementById("eitri-keys-hud")?.remove();
    if (removeInstallation === remove) removeInstallation = undefined;
  };
  removeInstallation = remove;
  return remove;
}
