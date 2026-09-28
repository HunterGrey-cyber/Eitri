/** Which of the panel's three keyboard states has the keys.
 *
 * `hint` is declared here and rendered by the status line's mode block, and nothing reaches it yet:
 * `f` lands in sub-project 4 along with counts, search, `]p`/`[p` and which-key. It is in the union
 * because the mode-block label table is data over this union, not because anything is stubbed. */
export type PanelMode = "browse" | "input" | "hint";

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
  | null;

/** The parts of a `KeyboardEvent` this decision needs. A plain object so the table is testable
 *  without a DOM. `keyCode` is read only for C4's legacy-WebKit IME check (some builds report an
 *  IME commit as `keyCode === 229` rather than `isComposing`); absent it is simply not that. */
export type KeyLike = { key: string; ctrlKey: boolean; shiftKey: boolean; isComposing: boolean; keyCode?: number };

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
    if (event.shiftKey && !event.ctrlKey && event.key === "D") return ctx.sessionEnded ? null : { kind: "deny-reason" };
    // C1a: `A` (`:h A`), named ahead of the blanket refusal the same reason G/N/Y/D are -- most
    // layouts deliver it with Shift held. Refused on an ended session for the same reason `i`/`o`
    // are just below: INPUT there has no box to place a caret in.
    if (event.shiftKey && !event.ctrlKey && event.key === "A") {
      return ctx.sessionEnded ? null : { kind: "mode", to: "input", caret: "end" };
    }
    // `Ctrl+g` in BROWSE views the current row in the editor. INPUT uses the same chord to edit
    // the draft, so this branch is gated on `mode !== "input"` to preserve the composer's action.
    if (event.ctrlKey && !event.shiftKey && event.key === "g") return { kind: "view-in-editor" };
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
  { keys: "Ctrl+c", what: "Interrupt the running turn (never closes anything)" },
  { keys: "a / d", what: "Allow / deny the card under the cursor or gating its tool call; only a lone key answers" },
  { keys: "Enter", what: "Show or hide a tool's result or a collapsed run" },
  {
    keys: "y / Y",
    what: "Copy the row (message, command, path), or the code block HINT landed on / its whole output",
  },
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
