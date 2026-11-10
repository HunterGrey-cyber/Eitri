import { effectiveRows, hasKeys, onRemove as registerReset, pluginBridge, removalResets, timeoutOf } from "./bridge";
import { blockedEvent, isEscape, shouldSkip, textField } from "./events";
import { installHeldSuperTracking, reset as resetSuper, superHeld } from "./held-super";
import { endHold, holdCode, setBottom } from "./glide";
import { clearFind, configureFind, findNext, openFind } from "./find";
import { activate, startHints } from "./hints";
import { setDimmed, setModeWords, showMessage, showMode, showStatus, showWhichKey } from "./hud";
import { canonicalOf, parseNotation, strokeOf, type KeyStroke } from "./notation";
import { createResolver } from "./resolver";
import { genericRows, isGenericAction, type Row } from "./rows";
import { runScroll } from "./scroll";
import { isOperable } from "./visible";
import { currentSettings, prefixKey, settingsOf, useSettings } from "./settings";
import { closeSheet, openSheet, sheetIsOpen, sheetKey } from "./sheet";
import { configureVisual, endVisual, startVisual, visualKey, visualKind, visualPending } from "./visual";
import type { InstallOptions, KitLabels } from "./index";

export type { HintTarget, InstallOptions } from "./index";

/** Registers a module reset for every installation removal; registrations survive reinstalls. */
export function onRemove(reset: () => void): void {
  registerReset(reset);
}

let current: Element | null = null;
let pageHasKeys = true;
let removeInstallation: (() => void) | undefined;
let generation = 0;
let installedRows: Row[] | null = null;

onRemove(() => {
  setCurrent(null);
  pageHasKeys = true;
  installedRows = null;
  removeInstallation = undefined;
  // Tickets remain unique so an earlier pending declaration cannot become current again.
  ++generation;
});

export function keysOf(action: string): string[] {
  return [...(installedRows?.find((row) => row.action === action)?.keys ?? [])];
}

const MODIFIER_KEYS = new Set([
  "Shift", "Control", "Alt", "AltGraph", "Meta", "Super", "Hyper", "CapsLock", "NumLock", "ScrollLock", "Fn", "OS",
]);

const ESCAPE: KeyStroke = { key: "Escape", ctrl: false, shift: false };

/** True when some binding begins with a Space without Ctrl: Space then belongs to the keys even on a focused control.
 *  With `shifted`, only a binding that begins with `<S-Space>` counts. */
export function spaceStartsBinding(rows: Row[], shifted = false): boolean {
  return rows.some((row) => row.keys.some((key) => {
    const first = parseNotation(key)?.[0];
    return first?.key === " " && !first.ctrl && (!shifted || first.shift);
  }));
}

export function setCurrent(el: Element | null): void {
  current?.classList.remove("eitri-keys-current", "eitri-keys-dimmed");
  current = el;
  current?.classList.add("eitri-keys-current");
  current?.classList.toggle("eitri-keys-dimmed", !pageHasKeys);
}

/** The generic rows, then the page's. A page row replaces the generic row of its action in place, and a key a page
 * row holds is taken from the other generic rows (a row left without keys is dropped), so no key names two actions. */
export function mergeRows(page: Row[], labels?: KitLabels, prefix?: string | null): Row[] {
  const banned = prefixKey(prefix);
  const spelled = (key: string) => canonicalOf(key) ?? key;
  const taken = new Set(page.flatMap((row) => row.keys.map(spelled)));
  const own = new Set(page.map((row) => row.action));
  const declared = new Map(genericRows(labels).map((row) => [row.action, row]));
  for (const row of page) declared.set(row.action, row);
  return [...declared.values()].flatMap((row) => {
    if (own.has(row.action)) return [row];
    const keys = row.keys.filter((key) => !taken.has(spelled(key)) &&
      (typeof prefix === "string" ? !banned || spelled(key) !== spelled(banned) : !parseNotation(key)?.[0].ctrl));
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
    // Visual mode ends with the keys, as vim's ends on leaving the window.
    if (!value) { endHold(); hints?.cancel(); endVisual(); }
    clearPrefix();
    if (!value) resetSuper();
    setDimmed(!value);
    current?.classList.toggle("eitri-keys-dimmed", !value);
  }));
  const config = bridge.on("keys_config", (data) => receive(() => {
    const value = timeoutOf(data);
    if (value !== undefined) { timeoutlen = value; clearPrefix(); }
  }));
  useSettings(settingsOf(opts.initial?.settings));
  let rows: ReturnType<typeof effectiveRows>;
  try {
    rows = effectiveRows(await bridge.call("keys.declare", { rows: mergeRows(opts.rows, opts.labels, opts.initial?.prefix) }));
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
  let whichKeyTimer: number | undefined;
  const resolver = createResolver(rows, () => timeoutlen, () => performance.now(),
    { prefixWait: () => currentSettings().prefixWait });
  const spaceToKeys = spaceStartsBinding(rows);
  const shiftSpaceToKeys = spaceStartsBinding(rows, true);
  const renderMode = () => {
    const visual = visualKind();
    showMode(textField(document.activeElement) ? "insert" : hints?.active ? "hint"
      : visual === "char" ? "visual" : visual === "line" ? "visual_line" : "normal",
    hints?.active ? hints.prefix : visual ? visualPending() : pending);
  };
  const stopWhichKey = () => {
    window.clearTimeout(whichKeyTimer);
    whichKeyTimer = undefined;
    showWhichKey("");
  };
  const clearPrefix = () => {
    window.clearTimeout(timer);
    timer = undefined;
    stopWhichKey();
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
  const onBlur = () => { endHold(); hints?.cancel(); endVisual(); clearPrefix(); };
  window.addEventListener("blur", onBlur);
  // A press of the pointer makes a selection of its own: visual mode gives way to it.
  const onPointerDown = () => {
    if (!visualKind()) return;
    endVisual();
    renderMode();
  };
  window.addEventListener("pointerdown", onPointerDown, true);
  // A held key's release ends its glide. Capture, so a page that stops the keyup cannot leave the view gliding.
  const onKeyUp = (ev: KeyboardEvent) => {
    const code = holdCode();
    if (code !== null && (ev.code || ev.key) === code) endHold();
  };
  window.addEventListener("keyup", onKeyUp, true);
  // A hidden page gets no keyup and no frames; the hold must not resume gliding when it comes back.
  const onVisibility = () => { if (document.visibilityState === "hidden") endHold(); };
  document.addEventListener("visibilitychange", onVisibility);
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

  const showContinuations = () => {
    whichKeyTimer = undefined;
    const items = resolver.continuations().map(({ rest, action }) =>
      `${rest} ${rows.find((row) => row.action === action)?.desc ?? action}`);
    if (items.length) showWhichKey(`${pending} → ${items.join(" · ")}`);
  };

  // The last key event the bubble listener (or the Space capture) handled, and the last one the window saw first.
  let handledEvent: KeyboardEvent | null = null;
  let observedEvent: KeyboardEvent | null = null;
  let observedTimer: number | undefined;
  // A key a page handler stopped before it bubbled to the kit went to the page, so it ends a waiting prefix as a key
  // the kit lets through does; otherwise a much later key could complete a sequence begun before it.
  const settleObserved = () => {
    window.clearTimeout(observedTimer);
    observedTimer = undefined;
    const ev = observedEvent;
    observedEvent = null;
    if (ev && ev !== handledEvent && pending && !MODIFIER_KEYS.has(ev.key)) clearPrefix();
  };
  // Capture, so a key every page handler may stop still ends a glide and is still accounted for.
  const onKeyFirst = (ev: KeyboardEvent) => {
    settleObserved();
    // Any key other than the held one's own repeats ends a glide, as the system stops repeating that key.
    if (!(ev.repeat && holdCode() === (ev.code || ev.key))) endHold();
    if (!pending) return;
    observedEvent = ev;
    // A timer runs only after this event's dispatch has finished, so by then the bubble listener has had its turn.
    observedTimer = window.setTimeout(settleObserved, 0);
  };

  const onKey = (ev: KeyboardEvent, replay = false) => {
    if (!replay) handledEvent = ev;
    if (!pageHasKeys) return;
    const held = superHeld();
    if (held) { clearPrefix(); return; }
    if (MODIFIER_KEYS.has(ev.key)) return;
    // With no timeout on a waiting prefix, a key that goes to the page or a field must end the wait, or a much later
    // key would complete a sequence begun long before.
    const letThrough = () => { if (pending) clearPrefix(); };
    // Buffered events already passed the guard; this listener prevented them while they were pending.
    if (!replay && shouldSkip(ev, held, spaceToKeys)) {
      // Only the resolver may authorize a Ctrl chord; the shell's navigation and IME chords stay reserved.
      if (blockedEvent(ev, held, spaceToKeys) || !ev.ctrlKey || ev.altKey || ev.metaKey || ev.shiftKey ||
        /^[hjkl]$/i.test(ev.key)) { letThrough(); return; }
    }
    const escape = isEscape(ev);
    const field = textField(document.activeElement);
    if (field) {
      if (escape) {
        field.blur();
        clearPrefix();
        ev.preventDefault();
      } else letThrough();
      return;
    }
    if (hints?.active) {
      if (!escape && (ev.ctrlKey || ev.altKey || ev.metaKey)) return;
      if (ev.repeat) { ev.preventDefault(); return; }
      const target = hints.feed(escape ? "Escape" : ev.key);
      if (target) activate(target);
      renderMode();
      ev.preventDefault();
      return;
    }
    if (sheetIsOpen()) {
      if (escape) { closeSheet(); clearPrefix(); ev.preventDefault(); }
      else if (!ev.ctrlKey && !ev.altKey && !ev.metaKey && sheetKey(ev.key)) {
        ev.preventDefault();
        if (!sheetIsOpen()) clearPrefix();
      }
      return;
    }
    // Visual mode has keys of its own (`visual.ts`); the page's table waits until it ends.
    if (visualKind()) {
      const took = visualKey(escape ? ESCAPE : strokeOf(ev), ev, () => timeoutlen,
        (action, event) => { runScroll(action, event); });
      renderMode();
      if (took) ev.preventDefault();
      return;
    }
    const result = resolver.feed(escape ? ESCAPE : strokeOf(ev));
    window.clearTimeout(timer);
    timer = undefined;
    stopWhichKey();
    pending = "";
    if (result.kind === "fallback") {
      runFallback(result.action, result.consumed);
      onKey(ev, replay);
      return;
    }
    if (result.kind === "cancelled") {
      clearPrefix();
      ev.preventDefault();
      return;
    }
    if (result.kind === "pending") {
      pendingEvents.push(ev);
      pendingEvents = pendingEvents.slice(-result.length);
      pending = result.prefix;
      const settings = currentSettings();
      if (resolver.hasFallback() || !settings.prefixWait) {
        timer = window.setTimeout(() => {
          const result = resolver.expire();
          if (result.kind === "fallback" && !superHeld()) runFallback(result.action, result.consumed);
          else clearPrefix();
        }, timeoutlen + 1);
      }
      if (settings.whichKeyDelay !== null) whichKeyTimer = window.setTimeout(showContinuations, settings.whichKeyDelay);
      renderMode();
      ev.preventDefault();
      return;
    }
    pendingEvents = [];
    renderMode();
    if (result.kind !== "action") return;
    if (perform(result.action, ev)) ev.preventDefault();
  };

  // A page element's own Space handler (a code line that toggles on Space, say) would otherwise take the key before
  // the bubbling listener, so a sequence that starts with Space could not start there.
  const onSpaceFirst = (ev: KeyboardEvent) => {
    // Shift+Space keeps its native meaning (page up, a control's own handler) unless a binding begins with it.
    if (ev.key !== " " || (ev.shiftKey && !shiftSpaceToKeys) || ev.ctrlKey || ev.altKey || ev.metaKey ||
      ev.isComposing || ev.keyCode === 229) return;
    if (ev.defaultPrevented || !pageHasKeys || superHeld() || ev.getModifierState("Super") ||
      ev.getModifierState("Hyper")) return;
    if (textField(document.activeElement) || hints?.active || sheetIsOpen()) return;
    ev.stopPropagation();
    // The key handling runs before the default is prevented, since it skips an event whose default is gone.
    onKey(ev);
    ev.preventDefault();
  };

  /** Runs `action` for the key event `ev` that completed it; true when the key was handled. */
  const perform = (action: string, ev: KeyboardEvent): boolean => {
    if (action === "scroll.down" || action === "scroll.up") {
      const active = document.activeElement;
      const list = active?.closest("[data-eitri-list]");
      if (list) {
        const items = [...list.querySelectorAll<HTMLElement>("a[href], button")].filter(isOperable);
        const index = items.indexOf(active as HTMLElement);
        const direction = action === "scroll.down" ? 1 : -1;
        const next = index < 0 ? (direction === 1 ? 0 : items.length - 1) : index + direction;
        for (let at = next, attempts = 0; at >= 0 && at < items.length && attempts < items.length;
          at += direction, attempts++) {
          const before = document.activeElement;
          items[at].focus({ preventScroll: true });
          if (document.activeElement !== before) {
            items[at].scrollIntoView?.({ block: "nearest" });
            break;
          }
        }
        return true;
      }
    }
    let handled: boolean;
    switch (action) {
      case "sheet.open": endHold(); openSheet(rows, opts.labels); handled = true; break;
      case "find.start": endHold(); openFind(); handled = true; break;
      case "find.next": case "find.prev": findNext(action === "find.next" ? 1 : -1); handled = true; break;
      case "visual.char": case "visual.line": {
        endHold();
        clearPrefix();
        startVisual(action === "visual.char" ? "char" : "line");
        renderMode();
        handled = true;
        break;
      }
      case "mode.escape": {
        endHold();
        clearFind();
        // The page sees focus as the key found it, so it can close what that focus is in and place focus itself.
        let took = false;
        try {
          took = opts.run("mode.escape", ev) === true;
        } catch (error) {
          console.warn("eitri keys: the page's mode.escape failed", error);
        }
        if (!took) leaveControl();
        handled = true;
        break;
      }
      case "hint.start": {
        if (ev.repeat) { handled = true; break; }
        endHold();
        clearPrefix();
        const rect = current?.getBoundingClientRect();
        const origin = rect ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } :
          { x: window.innerWidth / 2, y: window.innerHeight / 2 };
        hints = startHints(document, opts.targets?.() ?? [], origin, currentSettings().hintChars);
        renderMode();
        handled = true;
        break;
      }
      case "scroll.top": case "scroll.bottom": {
        // gg and G are jumps, as in vim: the page hears of them before and after, so it can keep a jump list. A
        // repeat of the key moves nothing.
        const jumps = !ev.repeat;
        if (jumps) tellJump("before");
        handled = runScroll(action, ev);
        if (jumps) tellJump("after");
        break;
      }
      default:
        handled = isGenericAction(action) ? runScroll(action, ev) : opts.run(action, ev);
    }
    return handled;
  };
  const tellJump = (when: "before" | "after") => {
    try {
      opts.onJump?.(when);
    } catch (error) {
      console.warn("eitri keys: the page's onJump failed", error);
    }
  };
  /** Moves focus off a focused control, as Vimium's Esc does: to the current item when the control is inside it, so
   *  the item's keys go on working, else nowhere. */
  const leaveControl = () => {
    const active = document.activeElement;
    if (!(active instanceof HTMLElement || active instanceof SVGElement) || active === document.body ||
      textField(active)) return;
    if (current instanceof HTMLElement && current !== active && current.contains(active)) {
      current.focus({ preventScroll: true });
      if (document.activeElement === current) return;
    }
    if (current !== active) active.blur();
  };
  configureFind({ root: opts.find?.root, exclude: opts.find?.exclude, messages: opts.labels?.messages, render: renderMode,
    jump: tellJump });
  configureVisual({ ...opts.visual, root: opts.find?.root, messages: opts.labels?.messages, ended: renderMode });
  setBottom(opts.bottom ?? null);
  window.addEventListener("keydown", onKeyFirst, true);
  window.addEventListener("keydown", onKey);
  if (spaceToKeys) window.addEventListener("keydown", onSpaceFirst, true);
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
    window.clearTimeout(observedTimer);
    stopWhichKey();
    window.removeEventListener("keydown", onKeyFirst, true);
    window.removeEventListener("keydown", onKey);
    window.removeEventListener("keydown", onSpaceFirst, true);
    window.removeEventListener("blur", onBlur);
    window.removeEventListener("pointerdown", onPointerDown, true);
    window.removeEventListener("keyup", onKeyUp, true);
    document.removeEventListener("visibilitychange", onVisibility);
    window.removeEventListener("scroll", onViewportChange, true);
    window.removeEventListener("resize", onViewportChange);
    document.removeEventListener("focusin", syncFocus);
    document.removeEventListener("focusout", syncFocus);
    stopSuper();
    focus();
    config();
    for (const reset of removalResets.slice().reverse()) reset();
  };
  removeInstallation = remove;
  return remove;
}
