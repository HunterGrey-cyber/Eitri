/** Which of the panel's three keyboard states has the keys.
 *
 * `hint` is declared here and rendered by the status line's mode block, and nothing reaches it yet:
 * `f` lands in sub-project 4 along with counts, search, `]p`/`[p` and which-key. It is in the union
 * because the mode-block label table is data over this union, not because anything is stubbed. */
export type PanelMode = "browse" | "input" | "hint";

export type PanelAction =
  | { kind: "mode"; to: PanelMode }
  /** `j`/`k`: to the next or previous stop (a row, a banner, the status line, ...), top to bottom.
   *  Which stop that is depends on the document, so `App.tsx` resolves it (`./nav`'s `nextStop`). */
  | { kind: "move"; delta: 1 | -1 }
  /** `h`/`l`: to the previous or next control inside the current stop (`./nav`'s `nextControl`). */
  | { kind: "control"; delta: 1 | -1 }
  /** `a`/`d`: answer the permission card under the cursor, or the one gating the tool call under it. */
  | { kind: "answer"; decision: "allow" | "deny" }
  | { kind: "toggle-expand" }
  | { kind: "copy" }
  | { kind: "restart" }
  /** `Ctrl+d` (+1) / `Ctrl+u` (-1): scroll the conversation by half its visible height. `App.tsx`
   *  measures the list and re-homes the cursor if the scroll carried its row out of sight. */
  | { kind: "half-page"; delta: 1 | -1 }
  /** `gg` (first) / `G` (last): the cursor to that end row, the list scrolled to that very end. */
  | { kind: "jump"; to: "first" | "last" }
  /** A lone `g`: nothing happens yet. `App.tsx` remembers it and passes it back in as
   *  `KeyContext.pendingG` with the next key, which is how `gg` is read without this table
   *  keeping any memory of its own. */
  | { kind: "pending-g" }
  /** `f` in BROWSE: start a global HINT. */
  | { kind: "hint" }
  /** `?` in BROWSE: open or close the full keymap (spec §3). */
  | { kind: "keymap" }
  | null;

/** The parts of a `KeyboardEvent` this decision needs. A plain object so the table is testable
 *  without a DOM. */
export type KeyLike = { key: string; ctrlKey: boolean; shiftKey: boolean; isComposing: boolean };

/** `sessionEnded` gates two rows in opposite directions: it is what OFFERS `r` (return to the start
 *  screen) and what REFUSES `i` (a dead session's composer is disabled, so INPUT has no box). The
 *  rule both serve is that no on-screen hint may promise a key the current mode drops. */
export type KeyContext = {
  sessionEnded: boolean;
  /** The previous key was a lone `g` in BROWSE and nothing has come since. Held by the caller
   *  (`App.tsx`), never by this table, so `resolveKey` stays a function of its three arguments.
   *  The caller clears it on every key, so any key other than a second `g` cancels it. */
  pendingG?: boolean;
};

/**
 * The key table, as a pure function: mode plus key plus context in, one action or nothing out.
 *
 * Nothing here touches the DOM or React, so every row is checkable without rendering, and the
 * component that owns focus (`App.tsx`) is left with only "apply this action".
 */
export function resolveKey(mode: PanelMode, event: KeyLike, ctx: KeyContext): PanelAction {
  // The three chords this table names, checked BEFORE the blanket modifier refusal below and before
  // the plain-key switch. The order is load-bearing: a browser reports `key === "d"` for Ctrl+d just
  // as for a bare `d` (Ctrl does not remap `key` the way Shift does), so a Ctrl+d that reached the
  // switch would land on `case "d"` and DENY a pending permission. Each chord is matched on its
  // exact modifier set -- Ctrl+Shift+d is none of them and is refused below like any other chord.
  // GTK claims none of these before the WebView (checked against `shell/src/main.rs` and
  // `shell/src/agent_panel.rs`, 2026-09-19): only a user's own Lua `keybinding` could.
  if (mode !== "input") {
    if (event.ctrlKey && !event.shiftKey && (event.key === "d" || event.key === "u")) {
      return { kind: "half-page", delta: event.key === "d" ? 1 : -1 };
    }
    if (event.shiftKey && !event.ctrlKey && event.key === "G") return { kind: "jump", to: "last" };
    // `?` arrives with Shift held on most layouts (Shift+/), so it must be named before the blanket
    // modifier refusal below, the same reason `G` is; matched on `key`, never on the physical key.
    if (event.key === "?" && !event.ctrlKey) return { kind: "keymap" };
  }
  // A key carrying a modifier this table does not name is not claimed. Apart from the three chords
  // just above, no row needs Ctrl or Shift, so any other chord holding either falls through
  // unclaimed -- an ordinary Shift+letter, and any Ctrl+letter GTK or a future binding wants, both
  // included. `KeyLike` has carried these two fields since the table was first written; this is what
  // makes reading them, rather than deleting them, the correct fix once something actually checked
  // whether they were used.
  if (event.ctrlKey || event.shiftKey) return null;
  if (mode === "input") {
    // An Esc mid-composition belongs to the input method -- fcitx uses it to cancel the preedit.
    // Eating it breaks pinyin, which this project verified end to end (P4) and must not regress.
    if (event.key === "Escape" && !event.isComposing) return { kind: "mode", to: "browse" };
    return null;
  }
  // browse and hint share these; hint adds its own in sub-project 4.
  switch (event.key) {
    case "i":
      // Refused on a dead session: the composer's textarea is `disabled` there, so INPUT has no box
      // to type into and resolves nothing but `Escape` -- entering it would drop `r`, the one key
      // the ended/lost rows actually promise. `Composer` stops offering its focusable placeholder at
      // the same moment, so no on-screen text promises a key this table has stopped resolving.
      return ctx.sessionEnded ? null : { kind: "mode", to: "input" };
    case "j":
      return { kind: "move", delta: 1 };
    case "k":
      return { kind: "move", delta: -1 };
    case "g":
      // `gg` is the only thing `g` begins; a lone `g` does nothing until the second one arrives.
      return ctx.pendingG === true ? { kind: "jump", to: "first" } : { kind: "pending-g" };
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
      // Only where the spec offers it: §3.2's disconnected/error row. Otherwise `r` is free for
      // sub-project 4 to claim.
      return ctx.sessionEnded ? { kind: "restart" } : null;
    case "f":
      return { kind: "hint" };
    case "Escape":
      return null;
    default:
      return null;
  }
}

/** One line of the `?` keymap: the key as a person types it, and what it does. */
export type KeyHelp = { keys: string; what: string };

/** BROWSE, i.e. everything `resolveKey` claims outside INPUT. Kept beside `resolveKey` and tied to
 *  it both ways by `keymap.test.ts`, so this list can neither promise a key that does nothing nor
 *  leave out one that does (spec §3.3). */
export const BROWSE_KEYS: KeyHelp[] = [
  { keys: "j / k", what: "Next / previous row; a long row scrolls first" },
  { keys: "h / l", what: "Previous / next button in the row" },
  { keys: "gg / G", what: "First / last row" },
  { keys: "Ctrl+d / Ctrl+u", what: "Half a page down / up" },
  { keys: "a / d", what: "Allow / deny the permission under the cursor" },
  { keys: "Enter", what: "Show or hide a tool's result" },
  { keys: "y", what: "Copy the row (or the code block HINT landed on)" },
  { keys: "i", what: "Start typing a message" },
  { keys: "f", what: "HINT: jump anywhere in the window" },
  { keys: "r", what: "New session, once this one has ended" },
  { keys: "?", what: "This list (?, Esc or q closes it)" },
];

export const INPUT_KEYS: KeyHelp[] = [
  { keys: "Enter", what: "Send" },
  { keys: "Shift+Enter", what: "New line" },
  { keys: "Esc", what: "Stop typing (back to browsing)" },
];

/** `shell`'s keys, not this page's: GTK takes them before the WebView sees them. Nothing here can
 *  check them against the code that binds them; `shell/src/main.rs`'s test
 *  `every_app_accelerator_is_in_the_panel_keymap` checks that each accelerator it registers is
 *  spelled here, which catches a key removed without this list changing, not one added. Bound in
 *  `shell/src/main.rs` (pane switch, top bar, HINT), `shell/src/window_mode.rs` (F11,
 *  Ctrl+Shift+F11) and `shell/src/agent_panel.rs` (Ctrl+Shift+R). */
export const WINDOW_KEYS: KeyHelp[] = [
  { keys: "Ctrl+h / Ctrl+l", what: "Editor / this panel" },
  { keys: "Ctrl+k", what: "Top bar (h / l move, Ctrl+j or Esc go back)" },
  { keys: "Ctrl+j", what: "Down to the terminal when it is shown, or a panel below (Ctrl+k comes back up)" },
  { keys: "Ctrl+Shift+F", what: "HINT from anywhere (f in this panel or the top bar)" },
  { keys: "F11", what: "Fullscreen" },
  { keys: "Ctrl+Shift+F11", what: "Immersive: fullscreen without the top bar" },
  { keys: "Ctrl+Shift+R", what: "Reload this panel (the session keeps running)" },
  { keys: "Ctrl+= / Ctrl++ / Ctrl+Keypad+", what: "Text size larger (both panes)" },
  { keys: "Ctrl+- / Ctrl+Keypad-", what: "Text size smaller (both panes)" },
  {
    keys: "Ctrl+0 / Ctrl+Keypad0 / Ctrl+Keypad0 (NumLock off)",
    what: "Text size reset (both panes)",
  },
];

/** `shell/src/prefix.rs`: tmux's own prefix, as the owner's tmux has it. */
export const PREFIX_KEYS: KeyHelp[] = [
  { keys: "Ctrl+a m / z", what: "Zoom this pane, or restore" },
  { keys: "Ctrl+a h / j / k / l", what: "Move a divider 5 cells (repeat within 500 ms)" },
  { keys: "Ctrl+a Ctrl+a", what: "Send Ctrl+a itself" },
  { keys: "Ctrl+a Ctrl+l", what: "Send Ctrl+l itself to the terminal (clear screen)" },
  { keys: "Ctrl+a t", what: "Terminal: show and focus it, or hide it when it has the keys" },
  { keys: "Ctrl+a = / -", what: "Text size larger / smaller, this pane only (repeat within 500 ms)" },
  { keys: "Ctrl+a 0", what: "Text size reset, this pane only" },
];
