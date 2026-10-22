import { superHeld } from "./heldSuper";
import { isModifierKey } from "./typingGuard";

/** Which of the panel's keyboard states has the keys.
 *
 * `hint` is declared here and rendered by the status line's mode block, and nothing reaches it yet:
 * `f` lands in sub-project 4 along with counts, search, `]p`/`[p` and which-key. It is in the union
 * because the mode-block label table is data over this union, not because anything is stubbed.
 *
 * `caret`/`visual`/`vline` (BROWSE caret and visual mode): a
 * precise-copy region entered from BROWSE with `v` (CARET, a moving block caret) or `V` (V-LINE
 * directly, O8's kept default); `v`/`V` from CARET start VISUAL/V-LINE. Fail-closed by design --
 * every BROWSE-only path in `App.tsx` already tests `mode === "browse"` rather than `mode !==
 * "input"`, so none of them act while any of the three is current unless named. */
export type PanelMode = "browse" | "input" | "hint" | "caret" | "visual" | "vline";

/** The first key of a two-key BROWSE sequence (`gg`, `[[`, `]]`, `]p`, `Ctrl+w h`), held by the caller between
 *  the two presses. See `KeyContext.pending`. `"C-w"` is `Ctrl+w` (vim's window prefix), spelled the way tmux spells
 *  a chord (`literal_key`'s own `"C-a"`), since a chord is not one character. */
export type PendingPrefix = "g" | "z" | "[" | "]" | "C-w";

/** Where `Ctrl+w h/j/k/l` sends the keys: a side of the panel, by geometry. */
export type PaneDirection = "left" | "down" | "up" | "right";

/** The panel's own which-key actions (`core::keymap::panel`'s
 *  `PanelAction::name()`), the ones a leader/table sequence can run. `tab.close-others` is
 *  LazyVim's `<leader>bo`, "Delete Other Buffers". Distinct from this file's own
 *  `PanelAction` union below -- that one is `resolveKey`'s single-key result, this one is a row in
 *  the table `resolveKey`/`leader.ts` look sequences up in. */
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
  | "mode.cycle"
  | "tab.stop-background";

/** One binding in the panel's key table, as `serialize_keymap_for_js`'s `panel.bindings` sends it:
 *  `keys` is vim notation split into tokens (`["<leader>", "b", "d"]`, `["H"]`), `source` says which
 *  layer of `defaults < nvim < init.lua` produced it. */
export type PanelBinding = { keys: string[]; action: PanelActionName; desc: string; source: "default" | "nvim" | "init.lua" };

/** The panel's whole which-key table, as `serialize_keymap_for_js` sends it on `keymap`.
 *  `leader`/`leaderLabel`/`leaderSource` come from nvim's `g:mapleader` through
 *  `keytrans` (unset/empty/unusable all fall back to Space, as LazyVim's default); `timeoutlen`/`timeout`
 *  are nvim's own options, read the same way (Global Constraint: before any report, Space/1000/on). */
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
  /** INPUT's `Ctrl+y` approves the active tab's oldest waiting card without leaving INPUT.
   *  `App.tsx` answers by `permissionId` after `TYPING_GUARD_MS`, only when the key stood alone,
   *  and preserves the composer. Denial stays on the card so a reason can be typed. */
  | { kind: "approve-oldest" }
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
  /** `Ctrl+e` (+1) / `Ctrl+y` (-1) in BROWSE scroll by one prose line height.
   *  A count repeats the scroll, unlike `half-page`. `App.tsx` scrolls a long tool result's
   *  capped box first, as `j`/`k` do, and uses `clampCursorToView` to re-home the cursor
   *  if its row leaves the view. */
  | { kind: "scroll-line"; delta: 1 | -1 }
  /** `Ctrl+f` / `PageDown` (+1) and `PageUp` (-1) (vim's
   *  `:help CTRL-F` and `:help CTRL-B`): scroll the conversation a whole view, keeping two of the old
   *  view's text lines on screen. Counted (`2 Ctrl+f` is two views), unlike `half-page`. `Ctrl+b` is the
   *  tmux prefix, not this table's, so `PageUp` is the one way back a page. `App.tsx` measures the list,
   *  scrolls it and re-homes the cursor the way `half-page` does (`clampCursorToView`). */
  | { kind: "page"; delta: 1 | -1 }
  /** `gg` (first) / `G` (last): the cursor to that end row, the list scrolled to that very end. */
  | { kind: "jump"; to: "first" | "last" }
  /** `zt` / `zz` / `zb` (vim's `:help zt`, `:help zz`, `:help zb`): scroll the
   *  conversation so the cursor's ROW -- the whole row, never a text line -- sits at the top, the
   *  middle or the bottom of the view. With a count the row is row N (`3zt`) and the cursor goes
   *  there too. `App.tsx` measures and scrolls; this table only names the edge. */
  | { kind: "scroll-row"; where: "top" | "center" | "bottom" }
  /** `zo` (`open: true`) / `zc` (`open: false`) (vim's `:help zo`, `:help zc`): open
   *  or close the fold under the cursor -- a tool's result, or a collapsed run of calls -- where `za`
   *  (`toggle-expand`, Enter's own action) flips it. Idempotent, as vim's are: a fold already the way
   *  it is asked stays as it is. */
  | { kind: "fold"; open: boolean }
  /** The first key of a two-key sequence: nothing happens yet. `App.tsx` remembers which prefix
   *  and passes it back in as `KeyContext.pending` with the next key, which is how `gg`/`[[`/`]]`/
   *  `Ctrl+w h` are read without this table keeping any memory of its own. */
  | { kind: "pending"; prefix: PendingPrefix }
  /** `Ctrl+w h` / `j` / `k` / `l` (vim's `:help CTRL-W_h` and its three
   *  neighbours): the keys go to the module on that side of the panel. `App.tsx` posts `pane_nav`, and
   *  shell runs the move `Ctrl+h/j/k/l` make from the panel (`main.rs`'s `move_focus`). `Ctrl+w j` is
   *  always the module below and never the composer -- `Ctrl+j` alone keeps that route (C1) -- and
   *  the panel changes nothing itself: a side with no module leaves the keys where they are. */
  | { kind: "pane"; direction: PaneDirection }
  /** A key that ran nothing on purpose, and `App.tsx` swallows it.
   *  `"unbound"`: the key after a reserved prefix completed none of its pairs (`FIXED_PAIRS`, or a
   *  table pair such as `[b`) -- vim's `clearopbeep`, so `g`, a pause, `d` can never deny a card.
   *  `"count-on-answer"`: `a`/`d`/`D` typed after a count (`3a`) -- a card answer takes no count, and
   *  `App.tsx` says so in the band (`COUNT_ANSWER_FLASH`) rather than answering. */
  | { kind: "cancel"; why: "unbound" | "count-on-answer" }
  /** A digit `App.tsx` accumulates into a count for `j`/`k`/`[[`/`]]` and the arrow keys.
   *  `G`/`gg`/`zt`/`zz`/`zb` use it as a row number; `gt`/`gT` use it as a tab number.
   *  A prefix passes the count to its second key. `Ctrl+f`/`PageDown`/`PageUp` repeat by it,
   *  and `]p`/`[p` move that many waiting cards. */
  | { kind: "count"; digit: number }
  /** `[[`/`]]`: move the cursor to the previous/next user prompt. */
  | { kind: "prompt-jump"; delta: 1 | -1 }
  /** `]p` (+1) / `[p` (-1) moves to the next/previous waiting card in this tab, wrapping at the ends.
   *  A count repeats the move. This only moves the cursor; `a`/`d` answer through the typing guard.
   *  `App.tsx` uses the timeline and session state to choose waiting, unanswered cards.
   *  This table has no timeline, so the pair resolves the same on an ended session. */
  | { kind: "card-jump"; delta: 1 | -1 }
  /** `Ctrl+c` interrupts a running turn. It is unclaimed while idle for native copy,
   *  and in INPUT, where the composer handles it. */
  | { kind: "interrupt" }
  /** `f` in BROWSE: start a global HINT. */
  | { kind: "hint" }
  /** `?` in BROWSE: open or close the full keymap. */
  | { kind: "keymap" }
  /** `c` in BROWSE (turn review): open the overlay over what changed on disk during this tab's latest
   *  turn. It decides nothing about the overlay's own keys -- those are `./review`'s -- and, like `?`, only
   *  opens: closing is the overlay's. */
  | { kind: "review" }
  /** `/` in BROWSE: open the incremental search, as vim's `/` does. */
  | { kind: "search" }
  /** `:` in BROWSE: open a vim-style command line in the
   *  footer that runs only `:trust` and `:untrust` -- any other line closes it with a flash, Esc
   *  silently -- so `:ls⏎`, `:l⏎` and `:d⏎` land in it and never reach a card. */
  | { kind: "ex-line" }
  /** `n` (+1) / `Shift+N` (-1) in BROWSE: repeat the last `/` search, wrapping as vim's `wrapscan` does. */
  | { kind: "search-next"; delta: 1 | -1 }
  /** `Ctrl+o` in either mode: toggle the detailed view with every result, wider cuts,
   *  and no collapsed runs. */
  | { kind: "detailed" }
  /** `zh` (-1) / `zl` (+1): scroll the current row's own table sideways (T1). */
  | { kind: "table-scroll"; delta: 1 | -1 }
  /** `gf` (vim's "go to file"): open the path(s) on the current row in the editor. */
  | { kind: "open-path" }
  /** `gx` (vim's `gx`, "open the URL under the cursor"): open the web link of
   *  the current row in the system browser. `App.tsx` reads the row's links off the DOM (this table has
   *  none): one whose visible text is its own address opens at once, several -- or a titled one, or one
   *  nobody can see -- wait for a letter, each shown in full; none flashes. Like `gf`, an ended session
   *  still has links to open. */
  | { kind: "open-link" }
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

/** The `KeyboardEvent` fields needed to resolve a key, represented as a plain object for DOM-free tests.
 *  `keyCode === 229` detects IME commits on older WebKit builds that omit `isComposing`.
 *  Optional `altKey`/`metaKey` fields default to not held. Card-answer keys read them;
 *  two-key table lookups and `FIXED_PAIRS` retain separate modifier handling.
 *  Super, Hyper and Meta have distinct masks in `shell/src/prefix.rs`.
 *  `getModifierState` exposes Super/Hyper when the engine reports them. It is optional
 *  so plain test objects that omit it still mean not held.
 *  WebKitGTK 2.52.6 exposes neither `metaKey` nor `getModifierState("Super")` for a held Super key,
 *  but delivers its keydown/keyup events. `heldSuper.ts` tracks them, and `hasSuperOrHyper`
 *  also reads that state. `hasAltGraph` uses `getModifierState("AltGraph")`; its behavior
 *  on real WebKitGTK is unverified. */
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

/** Whether Super or Hyper is held, read the only
 *  way this layer can (see `KeyLike`'s own doc comment on `getModifierState` for the honest caveat
 *  about whether WebKitGTK ever actually sets either). */
function hasSuperOrHyper(event: KeyLike): boolean {
  // WebKitGTK 2.52.6 reports neither on another key's event (sandbox pass, 2026-09-28), so the Super
  // key's own keydown/keyup, tracked by `heldSuper.ts`, is what actually catches it there.
  return (
    superHeld() || event.getModifierState?.("Super") === true || event.getModifierState?.("Hyper") === true
  );
}

/** Whether AltGr is held, read the same way
 *  `hasSuperOrHyper` reads Super/Hyper -- `getModifierState` is the only signal this layer has, and
 *  it carries the identical, honest caveat about real WebKitGTK. AltGr is a level-3 shift some
 *  layouts use to type an ordinary character (`@`, `€`); `typingGuard.ts`'s own `MODIFIER_KEYS`
 *  already lists a BARE `AltGraph` keydown as a modifier rather than a typed key, so treating a key
 *  held alongside it the same way `Ctrl`/`Alt`/`Meta` are is this predicate catching up to a
 *  decision this project had already made elsewhere, not a new one. */
function hasAltGraph(event: KeyLike): boolean {
  return event.getModifierState?.("AltGraph") === true;
}

/** v1 audit fixes, 2026-09-28: the single rule for "is this key a plain answer" -- no Ctrl, Alt,
 *  Meta, Super/Hyper or AltGraph held, and not a key an input method is still composing. Before
 *  this, the card-answer switch below (`a`/`d`/`D`) and `App.tsx`'s
 *  bypass-confirm `y` handler each
 *  hand-wrote their own version of this list; the latter never checked Super/Hyper at all, which is
 *  exactly the kind of drift two independent lists invite. Composing is folded in here too,
 *  duplicating `composerKeys.ts`'s `isImeKey` rather than importing it, the same reason
 *  `resolveKey`'s own top-of-function check does (`KeyLike` stays a plain object with no dependency
 *  of its own). AltGraph joined this list the same day, closing a gap neither surface had checked. Shift is deliberately
 *  absent: it is not a "modifier" for this predicate's purposes -- `D` answers with Shift held, and
 *  the bypass `y`'s own `Y` still counts, both decided by their own callers. */
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
  /** A pending two-key BROWSE prefix, held by `App.tsx` so `resolveKey` keeps no state.
   *  Outside INPUT, `resolvePendingSecond` handles a bare modifier, a matching second key
   *  or `Ctrl+c`; any other key, including a chord, cancels without running its own action.
   *  INPUT ignores this field. */
  pending?: PendingPrefix | null;
  /** The count `App.tsx` accumulates from `1`-`9` then `0`-`9`, or `null`/absent before any digit.
   *  This table reads it only to distinguish a lone `0` from one continuing a count.
   *  `App.tsx` applies it to `j`/`k`/`[[`/`]]`, row numbers for `G`/`gg`/`zt`/`zz`/`zb`,
   *  tab numbers for `gt`/`gT`, and repeated waiting-card moves for `]p`/`[p`.
   *  It has the row, tab and card information needed to apply those moves. */
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
  // Idle `Ctrl+c` is claimed in CARET to prevent native copy of the character under the caret.
  // `App.tsx` uses `vswallow` with `mode === "caret"` to show a hint to select text before copying.
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

/** Keys completing each reserved BROWSE prefix. Unmatched keys cancel in `resolvePendingSecond`
 *  without running their own action, so `g`, a pause, `d` cannot deny a card.
 *  `resolveKey` checks table pairs such as `[b`/`gt`/`gT` first. Add fixed pairs here
 *  and in `leader.ts`'s `FIXED_PENDING_ENTRIES` so the which-key box lists them.
 *  `leader.test.ts` checks that both tables contain the same keys. */
export const FIXED_PAIRS: Record<PendingPrefix, Readonly<Record<string, NonNullable<PanelAction>>>> = {
  // `gx`, vim's "open the link", beside `gf`'s "open the path".
  g: { g: { kind: "jump", to: "first" }, f: { kind: "open-path" }, x: { kind: "open-link" } },
  z: {
    h: { kind: "table-scroll", delta: -1 },
    l: { kind: "table-scroll", delta: 1 },
    // vim's `zt`/`zz`/`zb` (the row to the top/middle/bottom of the view) and
    // `za`/`zo`/`zc` (toggle/open/close a fold; `za` is Enter's own `toggle-expand`).
    t: { kind: "scroll-row", where: "top" },
    z: { kind: "scroll-row", where: "center" },
    b: { kind: "scroll-row", where: "bottom" },
    a: { kind: "toggle-expand" },
    o: { kind: "fold", open: true },
    c: { kind: "fold", open: false },
  },
  // `[p`/`]p`, the previous/next card waiting for an answer, beside the prompt pair.
  "[": { "[": { kind: "prompt-jump", delta: -1 }, p: { kind: "card-jump", delta: -1 } },
  "]": { "]": { kind: "prompt-jump", delta: 1 }, p: { kind: "card-jump", delta: 1 } },
  // vim's `CTRL-W h/j/k/l`, the module that way. `Ctrl+w w/p/o/q` and `Ctrl+w
  // Ctrl+h` are not here: they cancel, like every other key after this prefix.
  "C-w": {
    h: { kind: "pane", direction: "left" },
    j: { kind: "pane", direction: "down" },
    k: { kind: "pane", direction: "up" },
    l: { kind: "pane", direction: "right" },
  },
};

/** Resolve a key after a pending prefix. Bare modifiers do nothing yet; `Ctrl+c` interrupts
 *  a running turn and is unclaimed while idle. `gv` is reserved; `[]`/`][` do nothing.
 *  Pair completion rejects Ctrl/Shift but allows Alt/Meta, as bracket prefixes do,
 *  so layouts using Option for brackets can type `[[`/`]]`. Unmatched keys still cancel.
 *  `g`/`z` reject Alt/Meta on their first key but allow them on the second key. */
function resolvePendingSecond(prefix: PendingPrefix, event: KeyLike, ctx: KeyContext): PanelAction {
  if (isModifierKey(event.key)) return null;
  if (event.ctrlKey && !event.shiftKey && event.key === "c") return ctx.turnRunning === true ? { kind: "interrupt" } : null;
  if (!event.ctrlKey && !event.shiftKey) {
    // `hasOwn`, not a bare index: a key spelled like an `Object.prototype` member (`constructor`)
    // must be looked up in this table alone, never inherited into a pair.
    if (Object.prototype.hasOwnProperty.call(FIXED_PAIRS[prefix], event.key)) return FIXED_PAIRS[prefix][event.key];
    if (prefix === "g" && event.key === "v") return null;
    if ((prefix === "[" && event.key === "]") || (prefix === "]" && event.key === "[")) return null;
  }
  return { kind: "cancel", why: "unbound" };
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
  // The key after a pending prefix, outside INPUT (INPUT never reads one: a `g` left over from
  // BROWSE is simply dropped there). First, a second key completing a panel-table two-key sequence
  // whose first half is one of the four prefixes (`[b` -- a reserved prefix may start
  // a longer sequence than `FIXED_PAIRS` knows about), which applies to a second key typed with
  // Shift held too: vim's `gT` (`:help gT`) arrives as `key: "T"` with `shiftKey`.
  // Ctrl never completes a pair -- no table key is a Ctrl chord. Then whatever else
  // follows completes one of the fixed pairs or cancels. Both run here, ahead of every chord below
  // (`Ctrl+o` included: matched first, `g` then Ctrl+o toggled the detailed view)
  // and of the blanket modifier refusal, so no key ever runs as itself after a prefix.
  if (mode !== "input" && ctx.pending) {
    if (!event.ctrlKey) {
      const tableHit = ctx.table?.bindings.find((b) => b.keys.length === 2 && b.keys[0] === ctx.pending && b.keys[1] === event.key);
      if (tableHit) return { kind: "panel", binding: tableHit };
    }
    return resolvePendingSecond(ctx.pending, event, ctx);
  }
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
  // `panel/src/agent_panel.rs`, 2026-09-19): only a user's own Lua `keybinding` could.
  if (mode !== "input") {
    if (event.ctrlKey && !event.shiftKey && (event.key === "d" || event.key === "u")) {
      return { kind: "half-page", delta: event.key === "d" ? 1 : -1 };
    }
    // vim's own `Ctrl+e`/`Ctrl+y`, checked here for the same reason as Ctrl+d/
    // Ctrl+u just above -- `key` reports "e"/"y" the same under Ctrl as bare, and bare `y` (the
    // plain-key switch below) copies the current row, so an unchecked Ctrl+y would copy instead
    // of scrolling. INPUT's own `Ctrl+e` (end of line) is untouched: this whole block is gated on
    // `mode !== "input"` already.
    if (event.ctrlKey && !event.shiftKey && (event.key === "e" || event.key === "y")) {
      return { kind: "scroll-line", delta: event.key === "e" ? 1 : -1 };
    }
    // vim's `Ctrl+f`, a view down, named here for the same
    // reason as `Ctrl+d`/`Ctrl+e` -- `key` is "f" under Ctrl as bare, and a bare `f` starts a HINT (the
    // plain-key switch below). Its way back is `PageUp`: `Ctrl+b` is shell's tmux prefix by default,
    // taken by GTK before the page sees it, and stays unclaimed here. INPUT is untouched (this whole
    // block is `mode !== "input"`).
    if (event.ctrlKey && !event.shiftKey && event.key === "f") return { kind: "page", delta: 1 };
    // `Ctrl+w`, vim's window prefix, waits for `h`/`j`/`k`/`l` (`FIXED_PAIRS["C-w"]`)
    // with no timeout and cancels on anything else, like `g`/`z`/`[`/`]`. Named here for the same reason
    // as the chords above -- `key` is "w" under Ctrl as bare -- and on its exact modifier set: Shift, Alt,
    // Meta, Super/Hyper or AltGraph held is some other chord, which falls through unclaimed. INPUT keeps its
    // own `Ctrl+w` (delete a word: this whole block is `mode !== "input"`), and CARET/VISUAL swallow it
    // (`resolveRegionModifierKey`, never reaching here). GTK claims no `Ctrl+w` (`shell/src`, checked).
    if (
      event.ctrlKey &&
      !event.shiftKey &&
      !event.altKey &&
      !event.metaKey &&
      !hasSuperOrHyper(event) &&
      !hasAltGraph(event) &&
      event.key === "w"
    ) {
      return { kind: "pending", prefix: "C-w" };
    }
    // `Ctrl+c` interrupts a running turn without closing anything. While idle it is left to
    // the browser for native copy. INPUT's `Ctrl+c` belongs to the composer and is handled there.
    if (event.ctrlKey && !event.shiftKey && event.key === "c") return ctx.turnRunning === true ? { kind: "interrupt" } : null;
    if (event.shiftKey && !event.ctrlKey && event.key === "G") return { kind: "jump", to: "last" };
    // `?` arrives with Shift held on most layouts (Shift+/), so it must be named before the blanket
    // modifier refusal below, the same reason `G` is; matched on `key`, never on the physical key.
    if (event.key === "?" && !event.ctrlKey) return { kind: "keymap" };
    // `:` opens the command line before the modifier refusal because some layouts require Shift.
    // Ctrl, Alt or Meta makes it another chord, which falls through. A pending prefix handles
    // and cancels `:` above, so it never reaches this branch.
    if (event.key === ":" && !event.ctrlKey && !event.altKey && !event.metaKey) return { kind: "ex-line" };
    // `Shift+N` repeats the last `/` search backward. It is checked before the modifier refusal
    // because most layouts deliver capital `N` with Shift held, as they do `G`.
    if (event.shiftKey && !event.ctrlKey && event.key === "N") return { kind: "search-next", delta: -1 };
    // `Shift+Y`/`Shift+D` are checked before the blanket modifier refusal because most layouts
    // deliver them with Shift held. `D` requires a live session with a reason box to focus.
    if (event.shiftKey && !event.ctrlKey && event.key === "Y") return { kind: "copy-output" };
    // Shift+D uses `isPlainAnswerKey` to reject Ctrl/Alt/Meta/Super/Hyper/AltGraph.
    // This check precedes the blanket Ctrl/Shift refusal, which does not read the other modifiers.
    if (event.shiftKey && isPlainAnswerKey(event) && event.key === "D") {
      // A card answer takes no count (a count on a yes/no is a typo, never intent) -- `3D` is refused and says so,
      // never answered.
      if ((ctx.count ?? null) !== null) return { kind: "cancel", why: "count-on-answer" };
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
    // `resolvePendingSecond` reserves the unimplemented `gv` reselect action. Pending prefixes
    // return above this block, so a leftover `g` cannot reach plain `v` and start CARET.
    // A configured `gv` panel binding wins through the earlier table lookup.
    // BROWSE's `v` starts CARET; `V` starts V-LINE directly. Check them before the blanket
    // modifier refusal because most layouts deliver `V` with Shift held. Configured leader
    // or panel bindings take priority through `App.tsx`'s leader handling before this function.
    // `isPlainAnswerKey` rejects Ctrl/Alt/Meta/Super/Hyper/AltGraph, leaving those chords unclaimed.
    if (isPlainAnswerKey(event) && !event.shiftKey && event.key === "v") return { kind: "caret" };
    if (isPlainAnswerKey(event) && event.shiftKey && event.key === "V") return { kind: "visual", line: true };
  }
  // Unnamed Ctrl/Shift chords fall through unclaimed after the explicit chord handlers.
  // INPUT's `Ctrl+y` must be checked before that refusal so it can approve the active tab's
  // oldest waiting card without leaving the composer. Shift/Alt/Meta/Super/Hyper/AltGraph
  // are rejected; `hasSuperOrHyper` also reads held-key state. IME input has already returned.
  // `App.tsx` chooses the card and applies the typing guard; this table identifies the chord.
  // BROWSE's Ctrl+y scroll was handled above. Compare the letter without case because
  // Caps Lock can produce `key === "Y"` with Shift released; Shift itself is still rejected.
  if (
    mode === "input" &&
    event.key.toLowerCase() === "y" &&
    event.ctrlKey &&
    !event.shiftKey &&
    !event.altKey &&
    !event.metaKey &&
    !hasSuperOrHyper(event) &&
    !hasAltGraph(event)
  ) {
    return { kind: "approve-oldest" };
  }
  if (event.ctrlKey || event.shiftKey) return null;
  if (mode === "input") {
    // Esc during composition belongs to the input method, which uses it to cancel the preedit.
    // Consuming it here breaks pinyin input. The IME check at the top already handles this;
    // `!event.isComposing` keeps the same guard explicit in this branch.
    if (event.key === "Escape" && !event.isComposing) return { kind: "mode", to: "browse" };
    return null;
  }
  // Outside INPUT, a pending prefix is resolved above, before `Ctrl+o`, other chords and the
  // blanket modifier refusal. Table pairs and `FIXED_PAIRS` complete there; an unmatched key
  // cancels in `resolvePendingSecond` without running its ordinary action. Mismatched brackets
  // such as `[` then `]` do nothing instead of starting the other prefix.
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
      // rc.3 minors 6: a chord meant for something else (Alt+g, Meta+z) must not arm a prefix that
      // then swallows the next plain key. No layout types `g` or `z` with Alt or Meta held, unlike a
      // bracket (next case), so a modified one is simply not this key -- the same test `v`, `D` and
      // `a`/`d` apply. Unclaimed rather than cancelled: nothing is pending yet.
      if (!isPlainAnswerKey(event)) return null;
      return { kind: "pending", prefix: event.key };
    case "[":
    case "]":
      // Armed with Alt or Meta held on purpose: `event.key` being a bracket while Option is down IS
      // how a macOS German, French or Swiss layout types one, and `[[`/`]]` (completed by
      // `resolvePendingSecond`, which reads Alt and Meta the same way) must keep jumping there.
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
    // ArrowDown/ArrowUp share `j`/`k`'s cases so counts, movement through tall rows and the
    // typing guard all use the same `move` handling.
    case "j":
    case "ArrowDown":
      return { kind: "move", delta: 1 };
    case "k":
    case "ArrowUp":
      return { kind: "move", delta: -1 };
    case "l":
      return { kind: "control", delta: 1 };
    case "h":
      return { kind: "control", delta: -1 };
    // PageDown is `Ctrl+f`'s twin (named with the chords above),
    // PageUp the way back a page. Shift or Ctrl held never gets here: the blanket refusal above.
    case "PageDown":
      return { kind: "page", delta: 1 };
    case "PageUp":
      return { kind: "page", delta: -1 };
    case "a":
    case "d":
      // Card answers require `isPlainAnswerKey`: the blanket Ctrl/Shift refusal does not check
      // Alt/Meta, and this switch matches only `event.key`. Super/Hyper also require checks
      // because their masks are distinct from Meta in `shell/src/prefix.rs`.
      // If WebKitGTK exposes no DOM modifier for Super, checking the event cannot detect it;
      // handling that case requires a key controller in the shell ahead of the WebView.
      if (!isPlainAnswerKey(event)) return null;
      // A card answer takes no count -- `3a` is refused and says so, never answered.
      if ((ctx.count ?? null) !== null) return { kind: "cancel", why: "count-on-answer" };
      // A dead session's cards are inert: there is nobody left to answer.
      return ctx.sessionEnded ? null : { kind: "answer", decision: event.key === "a" ? "allow" : "deny" };
    case "Enter":
      return { kind: "toggle-expand" };
    case "y":
      return { kind: "copy" };
    case "r":
      // Only on the disconnected/error row, where a restart means something. Otherwise `r` is free for
      // sub-project 4 to claim.
      return ctx.sessionEnded ? { kind: "restart" } : null;
    case "f":
      return { kind: "hint" };
    case "c":
      // Turn review. Named like `a`/`d`: Alt+c, Meta+c, Super+c and AltGr+c are some other chord (Ctrl and
      // Shift never get here, the blanket refusal above), and `zc` is `FIXED_PAIRS`' own fold, resolved
      // before any of this. INPUT's `c` is a letter (the INPUT branch above returns first). It is offered
      // on an ended session too: what changed on disk is still there to read, and the overlay is read-only.
      return isPlainAnswerKey(event) ? { kind: "review" } : null;
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

/** The `?` sheet's section for the plugin page that held the keys when `prefix ?` was pressed: the plugin's
 *  title, whether the page has declared its keys, and its effective keys (`eitri_core::plugin::keys::plugin_help`).
 *  Title and rows come from the page, so they are drawn as text only. A declared page may still have no rows: it
 *  declared an empty table, or the user's overrides removed every key. */
export type PluginKeysHelp = { title: string; declared: boolean; rows: KeyHelp[] };

/** `open_keymap`'s `plugin` field when it has the shape the sheet draws, else `null`: a malformed section is
 *  left out rather than breaking the whole sheet. A missing `declared` reads as not declared, so the sheet never
 *  claims a table the host did not say it has. */
export function pluginKeysHelp(value: unknown): PluginKeysHelp | null {
  if (typeof value !== "object" || value === null) return null;
  const { title, declared, rows } = value as { title?: unknown; declared?: unknown; rows?: unknown };
  if (typeof title !== "string" || !Array.isArray(rows)) return null;
  if (declared !== undefined && typeof declared !== "boolean") return null;
  const ok = rows.every(
    (row) => typeof row === "object" && row !== null && typeof row.keys === "string" && typeof row.what === "string",
  );
  return ok ? { title, declared: declared === true, rows: rows.map((row) => ({ keys: row.keys, what: row.what })) } : null;
}

/** The row `shell`'s "Anywhere in the window" section prints for moving between modules (`eitri_core::keymap::root::
 *  help_rows`; a Rust test pins the words): its keys read `<left> / <right>`. The panel sits in the module to the
 *  right of the editor, so the second key is the one that brings the keyboard into it. */
const MODULE_MOVE_ROW = "Module left / right";

/** The key that brings the keyboard into this panel, read from the window keys the `?` sheet lists, or `null` when
 *  they list none: a companion window holds only the panel, and its one root row says the window manager decides
 *  where focus goes, and before the `keymap` envelope arrives there are no rows at all. A hint with no key to name
 *  is left out rather than guessed. */
export function panelEntryKey(windowKeys: KeyHelp[], companion: boolean): string | null {
  if (companion) return null;
  const row = windowKeys.find((help) => help.what === MODULE_MOVE_ROW);
  const keys = row?.keys.split(" / ").map((key) => key.trim());
  return keys !== undefined && keys.length === 2 && keys[1] !== "" ? keys[1] : null;
}

/** The PageUp row's text under the effective prefix (`KeymapHelp.prefix`, as a person reads it). vim's way
 *  back a page is `Ctrl+b`, which this panel never claims (`resolveKey`'s `Ctrl+f` note); the row says why
 *  only when `Ctrl+b` really is the prefix -- under another one (a user's `Ctrl+a`) naming `Ctrl+b` as
 *  "the prefix" was simply wrong, and naming that other key would explain nothing about PageUp. */
export function pageUpHelp(prefix: string): string {
  return prefix === "Ctrl+b" ? "A view up (Ctrl+b is the prefix)" : "A view up";
}

/** `BROWSE_KEYS` as the `?` overlay shows them under the effective prefix (only the PageUp row depends
 *  on it). */
export function browseKeys(prefix: string): KeyHelp[] {
  return BROWSE_KEYS.map((row) => (row.keys === "PageUp" ? { ...row, what: pageUpHelp(prefix) } : row));
}

/** BROWSE, i.e. everything `resolveKey` claims outside INPUT. Kept beside `resolveKey` and tied to
 *  it both ways by `keymap.test.ts`, so this list can neither promise a key that does nothing nor
 *  leave out one that does. */
export const BROWSE_KEYS: KeyHelp[] = [
  { keys: "j / k", what: "Next / previous row; a long row scrolls first" },
  { keys: "↓ / ↑", what: "The same as j / k" },
  { keys: "h / l", what: "Previous / next button in the row" },
  { keys: "gg / G", what: "First / last row" },
  { keys: "1-9", what: "A count: 3j three rows, 3G or 3gg row 3, 2]] two prompts, 2gt tab 2" },
  { keys: "[[ / ]]", what: "Previous / next prompt of yours" },
  { keys: "]p / [p", what: "Next / previous card waiting for an answer (wraps; the cursor moves, nothing is answered)" },
  { keys: "Ctrl+d / Ctrl+u", what: "Half a page down / up" },
  { keys: "Ctrl+e / Ctrl+y", what: "One line down / up (a count repeats it, e.g. 5 Ctrl+e)" },
  { keys: "Ctrl+f / PageDown", what: "A view down, keeping two lines (a count repeats it)" },
  { keys: "PageUp", what: pageUpHelp("Ctrl+b") },
  { keys: "Ctrl+c", what: "Interrupt the running turn (never closes anything)" },
  { keys: "a / d", what: "Allow / deny the card under the cursor or gating its tool call; only a lone key answers" },
  { keys: "Enter", what: "Show or hide a tool's result or a collapsed run" },
  {
    keys: "y / Y",
    what: "Copy the row (message, command, path), or the code block HINT landed on / its whole output",
  },
  { keys: "v / V", what: "A caret to move with h j k l, w b e, 0 $, gg G; V selects lines at once" },
  { keys: "D", what: "Deny with a reason: into the card's reason box, Enter denies (a Codex or ACP card has none: d denies)" },
  { keys: "i / o", what: "Start typing, caret where you left it (C1a: o is an exact alias of i)" },
  { keys: "A", what: "Start typing at the end of the draft" },
  { keys: "f", what: "HINT: jump anywhere in the window (links too; Enter opens one)" },
  { keys: "r", what: "Once this session has ended: resume it, or start a new one (its last row says which)" },
  { keys: "c", what: "Review: the files that changed on disk during this tab's latest turn (read-only)" },
  { keys: "/", what: "Search the conversation (Enter keeps the match, Esc goes back)" },
  { keys: ":", what: "A command line, as in vim: :trust and :untrust" },
  { keys: "n / N", what: "Next / previous match, wrapping" },
  { keys: "Ctrl+o", what: "Detailed view: every result, longer cuts, no collapsed runs" },
  { keys: "zh / zl", what: "Scroll this row's table left / right" },
  { keys: "zt / zz / zb", what: "This row to the top / middle / bottom of the view (3zt: row 3)" },
  { keys: "za / zo / zc", what: "Fold: toggle as Enter / open / close a result or a collapsed run" },
  { keys: "gf", what: "Open the path on this row in the editor (several: pick by letter)" },
  { keys: "gx", what: "Open this row's web link (several, or a titled one: pick by letter, each shown in full)" },
  {
    keys: "Ctrl+w h / Ctrl+w j / Ctrl+w k / Ctrl+w l",
    what: "The keys to the module that way, as Ctrl+h/j/k/l; Ctrl+w j is the module below, never the box",
  },
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

/** The turn review overlay's keys (`c` opens it), everything `./review`'s `resolveReviewKey` claims. Tied to
 *  that function both ways by `review.test.ts`, the discipline `BROWSE_KEYS` is held to. The overlay owns
 *  every key while it is open, so `a`/`d` and the rest of BROWSE's table never reach a card underneath.
 *  While a one-line prompt is open it owns them instead: `y`/`n`/`Escape` on a question, and the comment
 *  input's own text -- those are answers to the prompt, not rows of this table. */
export const REVIEW_KEYS: KeyHelp[] = [
  { keys: "j / k", what: "Next / previous file or hunk; an open hunk's box scrolls first" },
  { keys: "gg / G", what: "First / last stop" },
  { keys: "Ctrl+d / Ctrl+u", what: "Several stops down / up" },
  { keys: "Enter", what: "Open or close the file's hunks (on a hunk: close its file); on the folded group: open it; on an interrupted revert: restore it or forget it" },
  { keys: "x", what: "On a hunk: revert it. On a file row: revert the whole file (asks y/n). On a comment: delete it" },
  { keys: "u", what: "Undo the last revert made here" },
  { keys: "i", what: "Comment on the hunk under the cursor: a one-line input (Enter saves, Esc cancels)" },
  { keys: "s", what: "Send the draft's comments and reverts to the agent, after showing what will be sent (y sends, n cancels)" },
  { keys: "[ / ]", what: "Previous / next turn (turn scope)" },
  { keys: "S", what: "Switch between this turn and the whole session" },
  { keys: "o", what: "Open the file in the editor at the hunk's first line" },
  { keys: "y", what: "Copy path:line" },
  { keys: "q / Esc / c", what: "Close" },
];

export const INPUT_KEYS: KeyHelp[] = [
  { keys: "Enter", what: "Send; while a turn runs, into that turn (the agent reads it at its next step), or held for its end when the agent cannot take it" },
  { keys: "Ctrl+Enter", what: "Send now: interrupts a running turn, then sends what is held and this" },
  { keys: "Shift+Enter", what: "New line" },
  // Claude Code's own two newline keys, which used to send the draft.
  { keys: "Alt+Enter / \\ Enter", what: "New line (a backslash right before the caret is replaced by it)" },
  { keys: "↑ / ↓", what: "From the first / last line: the held messages back, then earlier prompts" },
  { keys: "Ctrl+r", what: "Search earlier prompts (Enter puts one in the box)" },
  // The `/` command menu (`Composer.tsx`): open while the box holds one line whose first word starts with `/`.
  { keys: "/ command", what: "Start the box with / for the command menu; typing narrows it, a space closes it" },
  { keys: "Ctrl+n / Ctrl+p", what: "In the command menu: next / previous command (↑ / ↓ too, ahead of prompt history)" },
  { keys: "Tab", what: "In the command menu: complete the selected command and a space; Enter does too when what you typed begins the name" },
  { keys: "Ctrl+w / Ctrl+u", what: "Delete a word / to the line's start" },
  { keys: "Ctrl+c", what: "Interrupt a running turn; idle, clear the box into history" },
  // An added key. The band names the card it would approve.
  { keys: "Ctrl+y", what: "Approve the oldest card waiting in this tab (the band names it); only a lone Ctrl+y answers" },
  { keys: "Ctrl+g", what: "Edit this in nvim (:wq brings it back, :q! changes nothing)" },
  { keys: "Ctrl+o", what: "Detailed view" },
  { keys: "Shift+Tab", what: "Cycle manual → auto → bypass (entering bypass asks first)" },
  { keys: "Esc", what: "Stop typing (back to browsing); with the command menu open, close it first" },
  { keys: "?", what: "This list, from an empty box" },
];

/** `shell`'s own keys, which nothing on this page can read: sent by `shell` in a `keymap` envelope
 *  on every `ready`, generated from `eitri_core::keymap` (the root table and the effective prefix
 *  table after `init.lua`). `prefix` is the prefix as a person reads it (`Ctrl+b`).
 *
 *  `serialize_keymap_for_js` also carries this panel's own which-key table (`panel`,
 *  `defaults < nvim < init.lua`'s `effective()`) and the prefix chord that opens a new tab,
 *  spelled out for the chooser's "New session" row (e.g. `"Ctrl+b c"`). */
export type KeymapHelp = {
  prefix: string;
  window: KeyHelp[];
  prefixKeys: KeyHelp[];
  panel: PanelTable;
  newTabChord: string;
  /** The lines of the user's tmux config the import skipped, for the overlay's last section. */
  tmuxSkipped?: KeyHelp[];
};
