import { superHeld } from "./heldSuper";
import { isModifierKey } from "./typingGuard";

/** Which panel keyboard state receives keys.
 *
 * `hint` has a status-line label but no entry path yet. It belongs in this union because
 * the mode-block label table uses the union as its data type.
 *
 * From BROWSE, v enters CARET with a moving block caret and V enters V-LINE directly.
 * In CARET, v/V start VISUAL/V-LINE for precise copying. BROWSE-only paths in `App.tsx`
 * require `mode === "browse"`, so they do not run in these modes unless explicitly handled. */
export type PanelMode = "browse" | "input" | "hint" | "caret" | "visual" | "vline";

/** The first key of a two-key BROWSE sequence (`gg`, `[[`, `]]`), held by the caller between the
 *  two presses. See `KeyContext.pending`. */
export type PendingPrefix = "g" | "z" | "[" | "]";

/** Action names for the panel's which-key table, matching `core::keymap::panel::PanelAction::name()`.
 *  `resolveKey` and `leader.ts` look up these names in table bindings. The `PanelAction` union
 *  below instead describes the result of resolving a key. */
export type PanelActionName =
  | "tab.new"
  | "tab.next"
  | "tab.prev"
  | "tab.last"
  | "tab.close"
  | "tab.close-others"
  | "tab.choose"
  | "tab.info"
  | "panel.search"
  | "panel.keymap"
  | "panel.handoff"
  | "mode.cycle";

/** One binding in the panel's key table, as `serialize_keymap_for_js`'s `panel.bindings` sends it:
 *  `keys` is vim notation split into tokens (`["<leader>", "b", "d"]`, `["H"]`), `source` says which
 *  layer of `defaults < nvim < init.lua` produced it. */
export type PanelBinding = { keys: string[]; action: PanelActionName; desc: string; source: "default" | "nvim" | "init.lua" };

/** The panel's which-key table, sent by `serialize_keymap_for_js` in a `keymap` message.
 *  Leader fields come from nvim's `g:mapleader` through `keytrans`; unset, empty or unusable
 *  values fall back to Space. `timeoutlen` and `timeout` come from nvim's options.
 *  Before any report, the defaults are Space, 1000 ms and timeout enabled. */
export type PanelTable = {
  leader: string;
  leaderLabel: string;
  leaderSource: "default" | "mapleader" | "unset" | "unusable";
  timeoutlen: number;
  timeout: boolean;
  bindings: PanelBinding[];
  groups: { keys: string[]; label: string }[];
};

/** The table's value before the first `keymap` envelope arrives: nvim's own defaults (Space,
 *  `timeoutlen` 1000, `timeout` on -- Global Constraint), no bindings yet. */
export const EMPTY_PANEL_TABLE: PanelTable = {
  leader: " ",
  leaderLabel: "Space",
  leaderSource: "default",
  timeoutlen: 1000,
  timeout: true,
  bindings: [],
  groups: [],
};

export type PanelAction =
  /** `Esc`/`Ctrl+k` leaving INPUT never carries a caret (there is nowhere left to place one); `i`/
   *  `o`/`A` entering INPUT always does: `"kept"` is the caret the box was left at
   *  (or the end of the draft, the first time), `"end"` is `A`'s own promise (`:h A`). */
  | { kind: "mode"; to: "browse" }
  | { kind: "mode"; to: "input"; caret: "kept" | "end" }
  /** BROWSE `Esc` reports that it does nothing while a turn runs; the turn keeps running.
   *  While idle, `Escape` is unclaimed. */
  | { kind: "esc-blocked" }
  /** `j`/`k`: to the next or previous stop (a row, a banner, the handoff area, ...), top to bottom.
   *  Which stop that is depends on the document, so `App.tsx` resolves it (`./nav`'s `nextStop`). */
  | { kind: "move"; delta: 1 | -1 }
  /** `h`/`l`: to the previous or next control inside the current stop (`./nav`'s `nextControl`). */
  | { kind: "control"; delta: 1 | -1 }
  /** `a`/`d`: answer only the permission card under the cursor or the one gating its tool call.
   *  `App.tsx` uses `./typingGuard` to answer only a key that stands alone. */
  | { kind: "answer"; decision: "allow" | "deny" }
  | { kind: "toggle-expand" }
  | { kind: "copy" }
  /** `Shift+Y`: copy a tool row's whole result, uncut, never anything else's. */
  | { kind: "copy-output" }
  | { kind: "restart" }
  /** `Shift+D`: focus the target card's reason box; `Enter` there denies with that reason. */
  | { kind: "deny-reason" }
  /** `Ctrl+d` (+1) / `Ctrl+u` (-1): scroll the conversation by half its visible height. `App.tsx`
   *  measures the list and re-homes the cursor if the scroll carried its row out of sight. */
  | { kind: "half-page"; delta: 1 | -1 }
  /** Ctrl+e (+1) / Ctrl+y (-1) scroll by one prose line height and accept a count.
   *  `App.tsx` first scrolls a long tool result's capped box, as j/k do, then uses
   *  `clampCursorToView` to re-home the cursor if its row leaves the view. */
  | { kind: "scroll-line"; delta: 1 | -1 }
  /** `gg` (first) / `G` (last): the cursor to that end row, the list scrolled to that very end. */
  | { kind: "jump"; to: "first" | "last" }
  /** The first key of a two-key sequence: nothing happens yet. `App.tsx` remembers which prefix
   *  and passes it back in as `KeyContext.pending` with the next key, which is how `gg`/`[[`/`]]`
   *  are read without this table keeping any memory of its own. */
  | { kind: "pending"; prefix: PendingPrefix }
  /** A digit `App.tsx` accumulates into a count for `j`/`k`/`[[`/`]]`. */
  | { kind: "count"; digit: number }
  /** `[[`/`]]`: move the cursor to the previous/next user prompt. */
  | { kind: "prompt-jump"; delta: 1 | -1 }
  /** `Ctrl+c` interrupts a running turn. It is unclaimed while idle for native copy,
   *  and in INPUT, where the composer handles it. */
  | { kind: "interrupt" }
  /** `f` in BROWSE: start a global HINT. */
  | { kind: "hint" }
  /** `?` in BROWSE: open or close the full keymap. */
  | { kind: "keymap" }
  /** `/` in BROWSE: open incremental search. */
  | { kind: "search" }
  /** `n` (+1) / `Shift+N` (-1) in BROWSE: repeat the last `/` search, wrapping. */
  | { kind: "search-next"; delta: 1 | -1 }
  /** `Ctrl+o` in either mode: toggle the detailed view with every result, wider cuts,
   *  and no collapsed runs. */
  | { kind: "detailed" }
  /** `zh` (-1) / `zl` (+1): scroll the current row's own table sideways (T1). */
  | { kind: "table-scroll"; delta: 1 | -1 }
  /** `gf`: open the path(s) on the current row in the editor. */
  | { kind: "open-path" }
  /** `Ctrl+g` in BROWSE: view the current row's whole text in an nvim scratch
   *  buffer. Distinct from INPUT's own `Ctrl+g`, which edits the composer's draft (`Composer.tsx`). */
  | { kind: "view-in-editor" }
  /** A `PanelBinding` reached through the panel's own which-key table (`./leader`), rather than a
   *  fixed row above. Reached today only as the second key of a two-key BROWSE prefix (`[b`); the
   *  leader itself and longer sequences run through `./leader`'s `startSequence`/`advanceSequence`,
   *  not this function. */
  | { kind: "panel"; binding: PanelBinding }
  /** `v` in BROWSE starts CARET, a moving block caret. `App.tsx` checks for a row under the cursor,
   *  excluding banners and Stop, because this table has no DOM to check for a starting point. */
  | { kind: "caret" }
  /** `V` in BROWSE starts V-LINE directly, skipping CARET. This is the only entry route
   *  using the `"visual"` action, and `line` is always `true`. */
  | { kind: "visual"; line: boolean }
  /** A counted motion inside CARET/VISUAL/V-LINE, applied by `App.tsx` through `./visual`'s
   *  `stepOnce`/`repeatMotion`. Both ends move in CARET; only `cursor` moves in VISUAL/V-LINE.
   *  `App.tsx` chooses which ends to move from the current mode. */
  | { kind: "vmove"; motion: VisualMotion }
  /** `o` inside VISUAL/V-LINE swaps `anchor` and `cursor`.
   *  It is unbound in CARET, where there is only one point. */
  | { kind: "vswap" }
  /** `y` inside VISUAL/V-LINE copies the highlighted text and ends the region, returning to BROWSE. */
  | { kind: "vyank" }
  /** `>` inside VISUAL/V-LINE quotes the highlighted text into the tab's draft and enters INPUT.
   *  `App.tsx` checks the selection, formats it with `./quote`, and updates the draft, mode and caret.
   *  This table only identifies the key. */
  | { kind: "vquote" }
  /** `v`/`V` from CARET starts VISUAL (`line: false`) or V-LINE (`line: true`).
   *  The other mode's key switches between VISUAL and V-LINE. Both set anchor = cursor = the caret.
   *  The current mode's own key uses `vback` instead. */
  | { kind: "vtoggle"; line: boolean }
  /** `Esc` or the current VISUAL/V-LINE mode's own `v`/`V` returns to CARET at the moving end.
   *  The region and its freeze stay active. `vend` instead ends the region and returns to BROWSE. */
  | { kind: "vback" }
  /** `Esc` in CARET, or an unbound plain key in a region mode, ends the region without copying.
   *  `key` is `"Escape"` for a silent exit; otherwise `App.tsx` uses the unbound key and mode
   *  to build an ended-mode hint. */
  | { kind: "vend"; key: string }
  /** Unclaimed Ctrl/Alt/Meta/Super/Hyper/AltGraph chords inside CARET/VISUAL/V-LINE are swallowed.
   *  The mode stays active. Idle `Ctrl+c` in CARET is also swallowed, with a hint to select text
   *  before copying. `App.tsx` gives no feedback for the other swallowed chords. */
  | { kind: "vswallow" }
  | null;

/** The motions shared by CARET/VISUAL/V-LINE and `./visual`: `h`/`l` by character,
 *  `j`/`k` by screen line, `w`/`e`/`b` by word, `0`/`$` to the hard line's start/end,
 *  and `gg`/`G` to the list's first/last selectable character. Declaring the type beside
 *  `PanelAction` lets `resolveKey` and `./visual` share the same union. */
export type VisualMotion = "h" | "l" | "j" | "k" | "w" | "e" | "b" | "0" | "$" | "gg" | "G";

/** The `KeyboardEvent` fields used here, kept as a plain object for tests without a DOM.
 *  `keyCode === 229` detects legacy WebKit IME events that may not set `isComposing`.
 *  Optional modifier fields and methods mean "not held" when absent. Card-answer keys check
 *  Alt and Meta; the two-key pair lookup keeps its separate modifier handling.
 *  Super and Hyper are distinct from Meta and are read through `getModifierState`.
 *  WebKitGTK 2.52.6 sets neither `metaKey` nor `getModifierState("Super")` while Super is held,
 *  but its keydown/keyup events arrive. `heldSuper.ts` tracks them for `hasSuperOrHyper`.
 *  The AltGraph modifier-state check has not been verified against real WebKitGTK. */
export type KeyLike = {
  key: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  isComposing: boolean;
  keyCode?: number;
  altKey?: boolean;
  metaKey?: boolean;
  getModifierState?: (key: string) => boolean;
};

/** Whether Super or Hyper is held according to `getModifierState`.
 *  These checks depend on WebKitGTK exposing the modifier state; see `KeyLike`. */
function hasSuperOrHyper(event: KeyLike): boolean {
  // WebKitGTK 2.52.6 reports neither on another key's event (sandbox pass, 2026-09-28), so the Super
  // key's own keydown/keyup, tracked by `heldSuper.ts`, is what actually catches it there.
  return (
    superHeld() || event.getModifierState?.("Super") === true || event.getModifierState?.("Hyper") === true
  );
}

/** Whether AltGr is held according to `getModifierState("AltGraph")`.
 *  AltGr can produce ordinary characters, but holding it must block plain answers.
 *  `typingGuard.ts` also treats bare AltGraph keydowns as modifiers.
 *  This modifier-state signal has not been verified against real WebKitGTK. */
function hasAltGraph(event: KeyLike): boolean {
  return event.getModifierState?.("AltGraph") === true;
}

/** Shared plain-answer check for a/d/D and bypass-confirm y: reject Ctrl, Alt, Meta,
 *  Super/Hyper, AltGraph and composing input so the two answer paths cannot drift apart.
 *  The IME check mirrors `composerKeys.ts`'s `isImeKey` without importing it, keeping
 *  `KeyLike` independent. Shift is allowed here because callers decide which shifted
 *  letters count, including D and bypass Y. */
export function isPlainAnswerKey(event: KeyLike): boolean {
  return (
    !event.ctrlKey &&
    !event.altKey &&
    !event.metaKey &&
    !hasSuperOrHyper(event) &&
    !hasAltGraph(event) &&
    !event.isComposing &&
    event.keyCode !== 229
  );
}

/** `sessionEnded` gates two rows in opposite directions: it is what OFFERS `r` (return to the start
 *  screen) and what REFUSES `i` (a dead session's composer is disabled, so INPUT has no box). The
 *  rule both serve is that no on-screen hint may promise a key the current mode drops. */
export type KeyContext = {
  sessionEnded: boolean;
  /** The previous key was the first of a two-key BROWSE sequence and nothing has come since. Held
   *  by the caller (`App.tsx`), never by this table, so `resolveKey` stays a function of its three
   *  arguments. The caller clears it on every key, so any key other than the matching second half
   *  cancels it. */
  pending?: PendingPrefix | null;
  /** The count `App.tsx` accumulates from `1`-`9` then `0`-`9`, or `null`/absent before any digit.
   *  This table reads it only to distinguish a lone `0` from one continuing a count.
   *  `App.tsx` applies the count to `j`/`k`/`[[`/`]]`, since it knows how often to repeat the move. */
  count?: number | null;
  /** Whether a turn is running in the active tab, for `Ctrl+c`.
   *  `undefined` and `false` both mean idle, including callers that omit this field. */
  turnRunning?: boolean;
  /** The panel's own which-key table, absent before the first `keymap`
   *  envelope. Consulted only for a second key completing a two-key BROWSE prefix (`[b`); `App.tsx`
   *  is expected to pass `KeymapHelp.panel` (or leave it unset), never `EMPTY_PANEL_TABLE` -- an
   *  empty table and an absent one behave identically here (no bindings to match). */
  table?: PanelTable;
};

/** Region-local pending `g` handling shared by CARET and VISUAL/V-LINE.
 *  `ctx.pending` carries this state separately from BROWSE's two-key prefix behavior.
 *  Returns an action for the initial `g` or `gg`, and nothing for the reserved `gv`.
 *  `"fallthrough"` drops the pending `g` and lets the caller handle any other key normally.
 *  `G` is a single keystroke handled by each caller's motion switch. */
function resolveRegionPendingG(event: KeyLike, ctx: KeyContext): PanelAction | "fallthrough" {
  if (ctx.pending === "g") {
    if (event.key === "g") return { kind: "vmove", motion: "gg" };
    // `gv` is reserved for reselecting a region, which is unimplemented. Swallow it so it cannot
    // fall through to `vtoggle`'s `v` handling.
    if (event.key === "v") return null;
    return "fallthrough";
  }
  if (event.key === "g") return { kind: "pending", prefix: "g" };
  return "fallthrough";
}

/** Modifier handling shared by CARET and VISUAL/V-LINE. `Ctrl+c` interrupts a running turn.
 *  `idleCtrlC` supplies the caller's idle behavior: CARET swallows it, while VISUAL/V-LINE
 *  leaves it to native copy. Other Ctrl/Alt/Meta/Super/Hyper/AltGraph chords are swallowed except `Ctrl+e`/`Ctrl+y`.
 *  Those reuse BROWSE's `scroll-line` action because region modes return before BROWSE's
 *  chord handling. `App.tsx` scrolls the frozen list one line without moving the caret
 *  or selection or ending the region. */
function resolveRegionModifierKey(event: KeyLike, ctx: KeyContext, idleCtrlC: PanelAction): PanelAction | "fallthrough" {
  if (isModifierKey(event.key)) return null;
  if (event.key === "c" && event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && !hasSuperOrHyper(event) && !hasAltGraph(event)) {
    return ctx.turnRunning === true ? { kind: "interrupt" } : idleCtrlC;
  }
  if (
    event.ctrlKey &&
    !event.shiftKey &&
    !event.altKey &&
    !event.metaKey &&
    !hasSuperOrHyper(event) &&
    !hasAltGraph(event) &&
    (event.key === "e" || event.key === "y")
  ) {
    return { kind: "scroll-line", delta: event.key === "e" ? 1 : -1 };
  }
  if (event.ctrlKey || event.altKey || event.metaKey || hasSuperOrHyper(event) || hasAltGraph(event)) {
    return { kind: "vswallow" };
  }
  return "fallthrough";
}

/**
 * CARET's own key table: a block caret, moved by the same motions
 * VISUAL/V-LINE use, that starts VISUAL/V-LINE rather than being either of them. Called by
 * `resolveKey` for `mode === "caret"`, never directly: every CARET key is decided here, with no
 * fallthrough to BROWSE/INPUT or to `resolveVisualKey` below it.
 */
function resolveCaretKey(event: KeyLike, ctx: KeyContext): PanelAction {
  // Swallow idle Ctrl+c in CARET so the engine cannot copy the character under the caret.
  // `App.tsx` combines `vswallow` with CARET mode to flash "nothing selected: v, then y".
  const modifierResult = resolveRegionModifierKey(event, ctx, { kind: "vswallow" });
  if (modifierResult !== "fallthrough") return modifierResult;
  // `?` ends CARET and opens the full keymap.
  if (event.key === "?") return { kind: "keymap" };
  const pendingResult = resolveRegionPendingG(event, ctx);
  if (pendingResult !== "fallthrough") return pendingResult;
  // `1`-`9` starts a count; `0`-`9` continues one. A lone `0` is a motion because CARET binds
  // it to the hard line's start, while BROWSE leaves it unclaimed.
  if (/^[0-9]$/.test(event.key)) {
    const digit = Number(event.key);
    if (digit === 0 && (ctx.count ?? null) === null) return { kind: "vmove", motion: "0" };
    return { kind: "count", digit };
  }
  switch (event.key) {
    case "h":
    case "l":
    case "j":
    case "k":
    case "w":
    case "e":
    case "b":
    case "$":
      return { kind: "vmove", motion: event.key as VisualMotion };
    case "G":
      return { kind: "vmove", motion: "G" };
    case "v":
      // CARET's `v`/`V` starts a selection with anchor = cursor = the caret.
      // CARET has no selection mode's own-key exit behavior.
      return { kind: "vtoggle", line: false };
    case "V":
      return { kind: "vtoggle", line: true };
    case "Escape":
      // CARET's `Esc` ends the whole region and returns to BROWSE on the caret's row.
      // `App.tsx` distinguishes it from an unbound key through `action.key === "Escape"`.
      return { kind: "vend", key: "Escape" };
    default:
      // Any other unbound plain key in CARET clears and ends the region without running its BROWSE
      // action. This includes `a`/`d`/`D`/`i`/`o`/Enter/Space.
      return { kind: "vend", key: event.key };
  }
}

/** VISUAL/V-LINE's key table, called by `resolveKey` with no fallthrough to BROWSE/INPUT.
 * `mode` distinguishes the current mode's own `v`/`V`, which returns to CARET (`vback`),
 * from the other key, which switches selection modes (`vtoggle`).
 * `vswap` and `vmove` carry enough information for `App.tsx` without that distinction. */
function resolveVisualKey(mode: "visual" | "vline", event: KeyLike, ctx: KeyContext): PanelAction {
  // Idle `Ctrl+c` in VISUAL/V-LINE is unclaimed so native copy uses the visible selection
  // and leaves the mode active. Bare modifiers, such as Shift before `$`/`V`, are also unclaimed.
  const modifierResult = resolveRegionModifierKey(event, ctx, null);
  if (modifierResult !== "fallthrough") return modifierResult;
  // `?` opens the full keymap. `App.tsx` ends VISUAL/V-LINE on this action, as it does
  // for other routes into the overlay.
  if (event.key === "?") return { kind: "keymap" };
  const pendingResult = resolveRegionPendingG(event, ctx);
  if (pendingResult !== "fallthrough") return pendingResult;
  // `1`-`9` starts a count; `0`-`9` continues one. A lone `0` is a motion because VISUAL binds
  // it to the hard line's start, while BROWSE leaves it unclaimed.
  if (/^[0-9]$/.test(event.key)) {
    const digit = Number(event.key);
    if (digit === 0 && (ctx.count ?? null) === null) return { kind: "vmove", motion: "0" };
    return { kind: "count", digit };
  }
  switch (event.key) {
    case "h":
    case "l":
    case "j":
    case "k":
    case "w":
    case "e":
    case "b":
    case "$":
      return { kind: "vmove", motion: event.key as VisualMotion };
    case "G":
      return { kind: "vmove", motion: "G" };
    case "o":
      return { kind: "vswap" };
    case "y":
      return { kind: "vyank" };
    case ">":
      return { kind: "vquote" };
    case "v":
      // VISUAL's own `v` returns to CARET; in V-LINE, `v` switches to VISUAL.
      return mode === "visual" ? { kind: "vback" } : { kind: "vtoggle", line: false };
    case "V":
      // V-LINE's own `V` returns to CARET; in VISUAL, `V` switches to V-LINE.
      return mode === "vline" ? { kind: "vback" } : { kind: "vtoggle", line: true };
    case "Escape":
      // `Esc` from VISUAL/V-LINE returns to CARET. The region and its freeze stay active.
      return { kind: "vback" };
    default:
      // Any other unbound plain key in VISUAL/V-LINE clears and ends the region without running
      // its BROWSE action. This includes `a`/`d`/`D`/Enter/Space.
      return { kind: "vend", key: event.key };
  }
}

/**
 * The key table, as a pure function: mode plus key plus context in, one action or nothing out.
 *
 * Nothing here touches the DOM or React, so every row is checkable without rendering, and the
 * component that owns focus (`App.tsx`) is left with only "apply this action".
 */
export function resolveKey(mode: PanelMode, event: KeyLike, ctx: KeyContext): PanelAction {
  // C4: a key the input method owns is never a command, in any mode -- checked before anything
  // else, including the chords right below, so an IME committing over a held Ctrl (rare, but not
  // impossible) still loses to the composition. `isComposing` is the modern signal; `keyCode === 229`
  // is what some WebKit builds still report for the same thing instead (`composerKeys.ts`'s
  // `isImeKey`, which this duplicates rather than imports so `KeyLike` stays a plain object with no
  // dependency of its own). INPUT's own, older `!event.isComposing` check below is now redundant
  // with this one and is kept only because deleting it would change nothing but the diff.
  if (event.isComposing || event.keyCode === 229) return null;
  // CARET/VISUAL/V-LINE have a separate branch after the IME check and before `Ctrl+o`.
  // The region swallows `Ctrl+o` so it cannot toggle the detailed view.
  // `resolveCaretKey`/`resolveVisualKey` handle every region key without falling through.
  if (mode === "caret") return resolveCaretKey(event, ctx);
  if (mode === "visual" || mode === "vline") return resolveVisualKey(mode, event, ctx);
  // `Ctrl+o` toggles the detailed view in both BROWSE and INPUT, so it is checked before
  // the BROWSE/HINT-only block and the blanket modifier refusal. This lets a reader inspect
  // folded content without leaving the composer.
  if (event.ctrlKey && !event.shiftKey && event.key === "o") return { kind: "detailed" };
  // The chords this table names outside INPUT, checked BEFORE the blanket modifier refusal below and
  // before the plain-key switch. The order is load-bearing: a browser reports `key === "d"` for
  // Ctrl+d just as for a bare `d` (Ctrl does not remap `key` the way Shift does), so a Ctrl+d that
  // reached the switch would land on `case "d"` and DENY a pending permission. Each chord is matched
  // on its exact modifier set -- Ctrl+Shift+d is none of them and is refused below like any other
  // chord. GTK claims none of these before the WebView (checked against `shell/src/main.rs` and
  // `shell/src/agent_panel.rs`, 2026-09-19): only a user's own Lua `keybinding` could.
  if (mode !== "input") {
    // Resolve panel-table pairs such as [b before the following chords and the blanket modifier refusal.
    // One lookup covers all four prefixes. Shifted second keys work too: gT arrives as
    // key "T" with Shift held. Ctrl never completes a pair because no table key is a Ctrl chord.
    if (ctx.pending && !event.ctrlKey) {
      const tableHit = ctx.table?.bindings.find((b) => b.keys.length === 2 && b.keys[0] === ctx.pending && b.keys[1] === event.key);
      if (tableHit) return { kind: "panel", binding: tableHit };
    }
    if (event.ctrlKey && !event.shiftKey && (event.key === "d" || event.key === "u")) {
      return { kind: "half-page", delta: event.key === "d" ? 1 : -1 };
    }
    // Handle `Ctrl+e`/`Ctrl+y` before the plain-key switch: Ctrl leaves `key` as "e"/"y",
    // so an unchecked Ctrl+y could copy the current row instead of scrolling. This block excludes
    // INPUT, where `Ctrl+e` belongs to the composer and moves to the line's end.
    if (event.ctrlKey && !event.shiftKey && (event.key === "e" || event.key === "y")) {
      return { kind: "scroll-line", delta: event.key === "e" ? 1 : -1 };
    }
    // `Ctrl+c` interrupts a running turn without closing anything. While idle it is left to
    // the browser for native copy. INPUT's `Ctrl+c` belongs to the composer and is handled there.
    if (event.ctrlKey && !event.shiftKey && event.key === "c") return ctx.turnRunning === true ? { kind: "interrupt" } : null;
    if (event.shiftKey && !event.ctrlKey && event.key === "G") return { kind: "jump", to: "last" };
    // `?` arrives with Shift held on most layouts (Shift+/), so it must be named before the blanket
    // modifier refusal below, the same reason `G` is; matched on `key`, never on the physical key.
    if (event.key === "?" && !event.ctrlKey) return { kind: "keymap" };
    // `Shift+N` repeats the last `/` search backward. It is checked before the modifier refusal
    // because most layouts deliver capital `N` with Shift held, as they do `G`.
    if (event.shiftKey && !event.ctrlKey && event.key === "N") return { kind: "search-next", delta: -1 };
    // `Shift+Y`/`Shift+D` are checked before the blanket modifier refusal because most layouts
    // deliver them with Shift held. `D` requires a live session with a reason box to focus.
    if (event.shiftKey && !event.ctrlKey && event.key === "Y") return { kind: "copy-output" };
    // Shift+D uses `isPlainAnswerKey` to reject Ctrl/Alt/Meta/Super/Hyper/AltGraph.
    // This check precedes the blanket Ctrl/Shift refusal, which does not read the other modifiers.
    if (event.shiftKey && isPlainAnswerKey(event) && event.key === "D") {
      return ctx.sessionEnded ? null : { kind: "deny-reason" };
    }
    // C1a: `A` (`:h A`), named ahead of the blanket refusal the same reason G/N/Y/D are -- most
    // layouts deliver it with Shift held. Refused on an ended session for the same reason `i`/`o`
    // are just below: INPUT there has no box to place a caret in.
    if (event.shiftKey && !event.ctrlKey && event.key === "A") {
      return ctx.sessionEnded ? null : { kind: "mode", to: "input", caret: "end" };
    }
    // `Ctrl+g` in BROWSE views the current row in the editor. INPUT uses the same chord to edit
    // the draft, so this branch is gated on `mode !== "input"` to preserve the composer's action.
    if (event.ctrlKey && !event.shiftKey && event.key === "g") return { kind: "view-in-editor" };
    // `gv` is reserved for reselecting a region, which is unimplemented. Handle it before plain `v`
    // so a pending `g` cannot start CARET. A configured `gv` panel binding wins because the table
    // lookup at the top of this block runs first.
    if (ctx.pending === "g" && event.key === "v" && !event.ctrlKey && !event.shiftKey) return null;
    // In BROWSE, v starts CARET and V starts V-LINE directly. Check before the blanket modifier
    // refusal because V carries Shift. Configured leader and panel bindings get first choice
    // in `App.tsx`. `isPlainAnswerKey` blocks Ctrl, Alt, Meta, Super/Hyper, AltGraph and
    // composing input from starting either mode.
    if (isPlainAnswerKey(event) && !event.shiftKey && event.key === "v") return { kind: "caret" };
    if (isPlainAnswerKey(event) && event.shiftKey && event.key === "V") return { kind: "visual", line: true };
  }
  // A key carrying a modifier this table does not name is not claimed. Apart from the chords just
  // above, no row needs Ctrl or Shift, so any other chord holding either falls through unclaimed --
  // an ordinary Shift+letter, and any Ctrl+letter GTK or a future binding wants, both included.
  // `KeyLike` has carried these two fields since the table was first written; this is what makes
  // reading them, rather than deleting them, the correct fix once something actually checked whether
  // they were used.
  if (event.ctrlKey || event.shiftKey) return null;
  if (mode === "input") {
    // Esc during composition belongs to the input method, which uses it to cancel the preedit.
    // Consuming it here breaks pinyin input. The IME check at the top already handles this;
    // `!event.isComposing` keeps the same guard explicit in this branch.
    if (event.key === "Escape" && !event.isComposing) return { kind: "mode", to: "browse" };
    return null;
  }
  // (A table pair completing a pending prefix, `[b`/`gt`/`gT`, is resolved above, ahead of the
  // chords and the blanket modifier refusal.)
  // The second key of a two-key sequence (`gg`, `[[`, `]]`). Anything that does not complete the
  // pending one falls through as an ordinary key below: the prefix is simply dropped, as vim drops
  // an unfinished `g`. A mismatched pair (`[` then `]`) is claimed as nothing rather than falling
  // through, so it cannot be misread as the OTHER prefix's own first half.
  switch (ctx.pending) {
    case "g":
      if (event.key === "g") return { kind: "jump", to: "first" };
      // `gf` opens the current row's path(s).
      if (event.key === "f") return { kind: "open-path" };
      break;
    case "[":
      if (event.key === "[") return { kind: "prompt-jump", delta: -1 };
      if (event.key === "]") return null;
      break;
    case "]":
      if (event.key === "]") return { kind: "prompt-jump", delta: 1 };
      if (event.key === "[") return null;
      break;
    case "z":
      // T1: `zh`/`zl` scroll the current row's own table sideways, the tool-result step's own
      // direction convention (`h` left, `l` right).
      if (event.key === "h") return { kind: "table-scroll", delta: -1 };
      if (event.key === "l") return { kind: "table-scroll", delta: 1 };
      break;
  }
  // A count for `j`/`k`/`[[`/`]]` starts with `1`-`9`; `0` only continues an existing count.
  // A lone `0` is unclaimed because no BROWSE command starts with it.
  if (/^[0-9]$/.test(event.key)) {
    const digit = Number(event.key);
    return digit === 0 && (ctx.count ?? null) === null ? null : { kind: "count", digit };
  }
  // browse and hint share these; hint adds its own in sub-project 4.
  switch (event.key) {
    case "g":
    case "z":
    case "[":
    case "]":
      return { kind: "pending", prefix: event.key };
    case "i":
    case "o":
      // Refused on a dead session: the composer's textarea is `disabled` there, so INPUT has no box
      // to type into and resolves nothing but `Escape` -- entering it would drop `r`, the one key
      // the ended/lost rows actually promise. `Composer` stops offering its focusable placeholder at
      // the same moment, so no on-screen text promises a key this table has stopped resolving.
      // C1a: `o` is an exact alias of `i` -- not vim's "open a line below" (`:h o`), decided now
      // because giving `o` a newline meaning later would change what it does today.
      return ctx.sessionEnded ? null : { kind: "mode", to: "input", caret: "kept" };
    case "j":
      return { kind: "move", delta: 1 };
    case "k":
      return { kind: "move", delta: -1 };
    case "l":
      return { kind: "control", delta: 1 };
    case "h":
      return { kind: "control", delta: -1 };
    case "a":
    case "d":
      // Alt or Meta with a/d must not answer a card: the blanket refusal checks only Ctrl/Shift,
      // and this switch matches `event.key` alone. `isPlainAnswerKey` shares the modifier and
      // composition checks with bypass confirmation. Super/Hyper need separate checks from Meta;
      // see `KeyLike` for the limitations of WebKitGTK's modifier signals.
      if (!isPlainAnswerKey(event)) return null;
      // A dead session's cards are inert: there is nobody left to answer.
      return ctx.sessionEnded ? null : { kind: "answer", decision: event.key === "a" ? "allow" : "deny" };
    case "Enter":
      return { kind: "toggle-expand" };
    case "y":
      return { kind: "copy" };
    case "r":
      // `r` resolves to `restart` only when `sessionEnded` is true; otherwise it is unclaimed.
      return ctx.sessionEnded ? { kind: "restart" } : null;
    case "f":
      return { kind: "hint" };
    case "/":
      return { kind: "search" };
    case "n":
      return { kind: "search-next", delta: 1 };
    case "Escape":
      // `Esc` never interrupts. While a turn runs it reports that; while idle it is unclaimed.
      return ctx.turnRunning === true ? { kind: "esc-blocked" } : null;
    default:
      return null;
  }
}

/** One line of the `?` keymap: the key as a person types it, and what it does. */
export type KeyHelp = { keys: string; what: string };

/** BROWSE, i.e. everything `resolveKey` claims outside INPUT. Kept beside `resolveKey` and tied to
 *  it both ways by `keymap.test.ts`, so this list can neither promise a key that does nothing nor
 *  leave out one that does. */
export const BROWSE_KEYS: KeyHelp[] = [
  { keys: "j / k", what: "Next / previous row; a long row scrolls first" },
  { keys: "h / l", what: "Previous / next button in the row" },
  { keys: "gg / G", what: "First / last row" },
  { keys: "1-9", what: "A count for j / k / [[ / ]] (3j moves three rows)" },
  { keys: "[[ / ]]", what: "Previous / next prompt of yours" },
  { keys: "Ctrl+d / Ctrl+u", what: "Half a page down / up" },
  { keys: "Ctrl+e / Ctrl+y", what: "One line down / up (a count repeats it, e.g. 5 Ctrl+e)" },
  { keys: "Ctrl+c", what: "Interrupt the running turn (never closes anything)" },
  { keys: "a / d", what: "Allow / deny the card under the cursor or gating its tool call; only a lone key answers" },
  { keys: "Enter", what: "Show or hide a tool's result or a collapsed run" },
  {
    keys: "y / Y",
    what: "Copy the row (message, command, path), or the code block HINT landed on / its whole output",
  },
  { keys: "v / V", what: "A caret to move with h j k l, w b e, 0 $, gg G; V selects lines at once" },
  { keys: "D", what: "Deny with a reason: into the card's reason box, Enter denies" },
  { keys: "i / o", what: "Start typing, caret where you left it (C1a: o is an exact alias of i)" },
  { keys: "A", what: "Start typing at the end of the draft" },
  { keys: "f", what: "HINT: jump anywhere in the window" },
  { keys: "r", what: "New session, once this one has ended" },
  { keys: "/", what: "Search the conversation (Enter keeps the match, Esc goes back)" },
  { keys: "n / N", what: "Next / previous match, wrapping" },
  { keys: "Ctrl+o", what: "Detailed view: every result, longer cuts, no collapsed runs" },
  { keys: "zh / zl", what: "Scroll this row's table left / right" },
  { keys: "gf", what: "Open the path on this row in the editor (several: pick by letter)" },
  { keys: "Ctrl+g", what: "This row's whole text in an nvim scratch buffer" },
  { keys: "?", what: "This list (?, Esc or q closes it)" },
];

/** CARET, i.e. everything `resolveCaretKey` claims. Tied to `resolveKey`'s
 *  CARET branch both ways by `keymap.test.ts`, the same discipline `BROWSE_KEYS` is held to.
 *  `KeymapOverlay`'s "Selecting" section renders this ahead of `VISUAL_KEYS`. */
export const CARET_KEYS: KeyHelp[] = [
  { keys: "h / l", what: "Previous / next character" },
  { keys: "j / k", what: "Next / previous screen line, same column" },
  { keys: "w / b / e", what: "Next word / previous word / end of this or the next word" },
  { keys: "0 / $", what: "Start / end of the line" },
  { keys: "gg / G", what: "First / last character of the conversation" },
  { keys: "1-9", what: "A count for the next motion (3w moves three words)" },
  { keys: "v / V", what: "Select from here: VISUAL by character, V-LINE by line" },
  { keys: "Ctrl+e / Ctrl+y", what: "Scroll the list one line down / up (a count repeats it); the caret stays put" },
  { keys: "Esc", what: "Back to BROWSE, cursor on this row" },
  { keys: "?", what: "This list (any other key ends the caret too, except Ctrl+e/Ctrl+y, which scroll instead)" },
];

/** VISUAL/V-LINE, i.e. everything `resolveVisualKey` claims. Tied to
 *  `resolveKey`'s VISUAL/V-LINE branch both ways by `keymap.test.ts`, the same discipline
 *  `BROWSE_KEYS` is held to. `KeymapOverlay` renders this after `CARET_KEYS`, both inside the one
 *  "Selecting" section, once `v`/`V` is reachable at all. */
export const VISUAL_KEYS: KeyHelp[] = [
  { keys: "h / l", what: "Previous / next character" },
  { keys: "j / k", what: "Next / previous screen line, same column" },
  { keys: "w / b / e", what: "Next word / previous word / end of this or the next word" },
  { keys: "0 / $", what: "Start / end of the line" },
  { keys: "gg / G", what: "Extend to the conversation's first / last character" },
  { keys: "1-9", what: "A count for the next motion (3w moves three words)" },
  { keys: "o", what: "Swap which end of the selection moves" },
  { keys: "v / V", what: "Switch to VISUAL / V-LINE; the mode's own key goes back to the caret" },
  { keys: "y", what: "Copy the highlighted text and return to BROWSE" },
  { keys: ">", what: "Quote into the message below, then type" },
  { keys: "Esc", what: "Back to the caret, at the moving end" },
  { keys: "Ctrl+e / Ctrl+y", what: "Scroll the list one line down / up (a count repeats it); the selection stays put" },
  { keys: "?", what: "This list (any other key ends the region too, except Ctrl+e/Ctrl+y, which scroll instead)" },
];

export const INPUT_KEYS: KeyHelp[] = [
  { keys: "Enter", what: "Send; while a turn runs, queue it for the turn's end" },
  { keys: "Ctrl+Enter", what: "Send now: interrupts a running turn, then sends the queue and this" },
  { keys: "Shift+Enter", what: "New line" },
  { keys: "↑ / ↓", what: "From the first / last line: the queue back, then earlier prompts" },
  { keys: "Ctrl+r", what: "Search earlier prompts (Enter puts one in the box)" },
  { keys: "Ctrl+w / Ctrl+u", what: "Delete a word / to the line's start" },
  { keys: "Ctrl+c", what: "Interrupt a running turn; idle, clear the box into history" },
  { keys: "Ctrl+g", what: "Edit this in nvim (:wq brings it back, :q! changes nothing)" },
  { keys: "Ctrl+o", what: "Detailed view" },
  { keys: "Shift+Tab", what: "Toggle auto ⇄ bypass (entering bypass asks first)" },
  { keys: "Esc", what: "Stop typing (back to browsing)" },
  { keys: "?", what: "This list, from an empty box" },
];

/** Key help sent by `shell` in a `keymap` envelope on every `ready`.
 *  It contains the core root table and effective prefix table after `init.lua`.
 *  `prefix` is the readable chord, such as `Ctrl+b`.
 *  `serialize_keymap_for_js` also includes the effective panel which-key table,
 *  with precedence `defaults < nvim < init.lua`, and `newTabChord`, the chord shown
 *  on the chooser's "New session" row, such as `Ctrl+b c`. */
export type KeymapHelp = { prefix: string; window: KeyHelp[]; prefixKeys: KeyHelp[]; panel: PanelTable; newTabChord: string };
