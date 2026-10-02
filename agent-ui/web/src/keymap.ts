import { superHeld } from "./heldSuper";
import { isModifierKey } from "./typingGuard";

/** Which of the panel's keyboard states has the keys.
 *
 * `hint` is declared here and rendered by the status line's mode block, and nothing reaches it yet:
 * `f` lands in sub-project 4 along with counts, search, `]p`/`[p` and which-key. It is in the union
 * because the mode-block label table is data over this union, not because anything is stubbed.
 *
 * `caret`/`visual`/`vline` (BROWSE caret and visual mode, spec
 * `docs/superpowers/specs/2026-09-28-browse-visual-mode-design.md`, revised for item 3a, D1): a
 * precise-copy region entered from BROWSE with `v` (CARET, a moving block caret) or `V` (V-LINE
 * directly, O8's kept default); `v`/`V` from CARET start VISUAL/V-LINE. Fail-closed by design --
 * every BROWSE-only path in `App.tsx` already tests `mode === "browse"` rather than `mode !==
 * "input"`, so none of them act while any of the three is current unless named. */
export type PanelMode = "browse" | "input" | "hint" | "caret" | "visual" | "vline";

/** The first key of a two-key BROWSE sequence (`gg`, `[[`, `]]`, `]p`, `Ctrl+w h`), held by the caller between
 *  the two presses. See `KeyContext.pending`. `"C-w"` is `Ctrl+w` (v1 picks, Task 6, ruling R11: vim's
 *  window prefix), spelled the way tmux spells a chord (`literal_key`'s own `"C-a"`), since a chord is
 *  not one character. */
export type PendingPrefix = "g" | "z" | "[" | "]" | "C-w";

/** Where `Ctrl+w h/j/k/l` sends the keys (v1 picks, Task 6): a side of the panel, by geometry. */
export type PaneDirection = "left" | "down" | "up" | "right";

/** The panel's own which-key actions (panel round 2 plan, Task 1: `core::keymap::panel`'s
 *  `PanelAction::name()`), the ones a leader/table sequence can run. `tab.close-others` is the
 *  owner's Q2 ruling (`<leader>bo`, LazyVim's "Delete Other Buffers"). Distinct from this file's own
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
  | "mode.cycle";

/** One binding in the panel's key table, as `serialize_keymap_for_js`'s `panel.bindings` sends it:
 *  `keys` is vim notation split into tokens (`["<leader>", "b", "d"]`, `["H"]`), `source` says which
 *  layer of `defaults < nvim < init.lua` produced it (spec §3.6). */
export type PanelBinding = { keys: string[]; action: PanelActionName; desc: string; source: "default" | "nvim" | "init.lua" };

/** The panel's whole which-key table, as `serialize_keymap_for_js` sends it on `keymap` (panel round
 *  2 plan, Task 5/6). `leader`/`leaderLabel`/`leaderSource` come from nvim's `g:mapleader` through
 *  `keytrans` (Global Constraint: unset/empty/unusable all fall back to Space); `timeoutlen`/`timeout`
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
   *  `o`/`A` entering INPUT always does (spec §3.2, C1a): `"kept"` is the caret the box was left at
   *  (or the end of the draft, the first time), `"end"` is `A`'s own promise (`:h A`). */
  | { kind: "mode"; to: "browse" }
  | { kind: "mode"; to: "input"; caret: "kept" | "end" }
  /** BROWSE `Esc` while a turn runs (spec §4.2, R34): a no-op that says so, rather than silently
   *  doing nothing -- `Esc` still never interrupts (D1) and the turn keeps running. Idle, plain
   *  `Escape` resolves to nothing at all (unclaimed), the same as always. */
  | { kind: "esc-blocked" }
  /** `j`/`k`: to the next or previous stop (a row, a banner, the handoff area, ...), top to bottom.
   *  Which stop that is depends on the document, so `App.tsx` resolves it (`./nav`'s `nextStop`). */
  | { kind: "move"; delta: 1 | -1 }
  /** `h`/`l`: to the previous or next control inside the current stop (`./nav`'s `nextControl`). */
  | { kind: "control"; delta: 1 | -1 }
  /** `a`/`d`: answer the permission card under the cursor, or the one gating the tool call under it
   *  -- nothing else since v1 S4 (spec 2026-09-27 §2.2; P1's "the only card, from any row" is gone).
   *  `App.tsx` answers only a key that stands alone (S1, `./typingGuard`). */
  | { kind: "answer"; decision: "allow" | "deny" }
  /** INPUT's `Ctrl+y` (owner decision #39, 2026-09-30): approve the ACTIVE tab's OLDEST waiting card
   *  -- the one a card landing takes (R11) -- without leaving INPUT. `App.tsx` answers it by that
   *  card's own `permissionId`, `TYPING_GUARD_MS` later and only when the key stood alone (S1), and
   *  leaves the composer as it was. No deny counterpart: deny stays where a reason can be typed. */
  | { kind: "approve-oldest" }
  | { kind: "toggle-expand" }
  | { kind: "copy" }
  /** `Shift+Y` (N3): a tool row's whole result, uncut, never anything else's. */
  | { kind: "copy-output" }
  | { kind: "restart" }
  /** `Shift+D` (P5): the keys go to the target card's reason box; `Enter` there denies with it. */
  | { kind: "deny-reason" }
  /** `Ctrl+d` (+1) / `Ctrl+u` (-1): scroll the conversation by half its visible height. `App.tsx`
   *  measures the list and re-homes the cursor if the scroll carried its row out of sight. */
  | { kind: "half-page"; delta: 1 | -1 }
  /** `Ctrl+e` (+1) / `Ctrl+y` (-1) (v1 trial item 5, owner: "能不能给browse 加上contrl e/y",
   *  copying vim's own `:help CTRL-E`/`:help CTRL-Y`): scroll the conversation by one text line --
   *  the prose line height -- rather than half a view. Counted (`5 Ctrl+e`, R4's own count),
   *  unlike `half-page` just above, which never was. `App.tsx` scrolls a long tool result's own
   *  capped box first, exactly the way `j`/`k` do, and re-homes the cursor the same way
   *  `half-page` does (reusing `clampCursorToView`) if the scroll carried its row out of sight. */
  | { kind: "scroll-line"; delta: 1 | -1 }
  /** `Ctrl+f` / `PageDown` (+1) and `PageUp` (-1) (v1 picks, Task 5, the owner's decision #13; vim's
   *  `:help CTRL-F` and `:help CTRL-B`): scroll the conversation a whole view, keeping two of the old
   *  view's text lines on screen. Counted (`2 Ctrl+f` is two views), unlike `half-page`. `Ctrl+b` is the
   *  tmux prefix, not this table's, so `PageUp` is the one way back a page. `App.tsx` measures the list,
   *  scrolls it and re-homes the cursor the way `half-page` does (`clampCursorToView`). */
  | { kind: "page"; delta: 1 | -1 }
  /** `gg` (first) / `G` (last): the cursor to that end row, the list scrolled to that very end. */
  | { kind: "jump"; to: "first" | "last" }
  /** `zt` / `zz` / `zb` (v1 picks, Task 4; vim's `:help zt`, `:help zz`, `:help zb`): scroll the
   *  conversation so the cursor's ROW -- the whole row, never a text line -- sits at the top, the
   *  middle or the bottom of the view. With a count the row is row N (`3zt`) and the cursor goes
   *  there too. `App.tsx` measures and scrolls; this table only names the edge. */
  | { kind: "scroll-row"; where: "top" | "center" | "bottom" }
  /** `zo` (`open: true`) / `zc` (`open: false`) (v1 picks, Task 4; vim's `:help zo`, `:help zc`): open
   *  or close the fold under the cursor -- a tool's result, or a collapsed run of calls -- where `za`
   *  (`toggle-expand`, Enter's own action) flips it. Idempotent, as vim's are: a fold already the way
   *  it is asked stays as it is. */
  | { kind: "fold"; open: boolean }
  /** The first key of a two-key sequence: nothing happens yet. `App.tsx` remembers which prefix
   *  and passes it back in as `KeyContext.pending` with the next key, which is how `gg`/`[[`/`]]`/
   *  `Ctrl+w h` are read without this table keeping any memory of its own. */
  | { kind: "pending"; prefix: PendingPrefix }
  /** `Ctrl+w h` / `j` / `k` / `l` (v1 picks, Task 6, ruling R11; vim's `:help CTRL-W_h` and its three
   *  neighbours): the keys go to the module on that side of the panel. `App.tsx` posts `pane_nav`, and
   *  shell runs the move `Ctrl+h/j/k/l` make from the panel (`main.rs`'s `move_focus`). `Ctrl+w j` is
   *  always the module below and never the composer -- `Ctrl+j` alone keeps that route (C1) -- and
   *  the panel changes nothing itself: a side with no module leaves the keys where they are. */
  | { kind: "pane"; direction: PaneDirection }
  /** K01 (= X-A-11, ruling R1/R2): a key that ran nothing on purpose, and `App.tsx` swallows it.
   *  `"unbound"`: the key after a reserved prefix completed none of its pairs (`FIXED_PAIRS`, or a
   *  table pair such as `[b`) -- vim's `clearopbeep`, so `g`, a pause, `d` can never deny a card.
   *  `"count-on-answer"`: `a`/`d`/`D` typed after a count (`3a`) -- a card answer takes no count, and
   *  `App.tsx` says so in the band (`COUNT_ANSWER_FLASH`) rather than answering. */
  | { kind: "cancel"; why: "unbound" | "count-on-answer" }
  /** A digit `App.tsx` accumulates into a count for `j`/`k`/`[[`/`]]` (R4) and, since the v1 picks'
   *  R2, for `G`/`gg`/`zt`/`zz`/`zb` (row N) and `gt`/`gT` (tab N) -- a prefix hands it on to its
   *  second key. `Ctrl+f`/`PageDown`/`PageUp` (Task 5) repeat by it as well, and the arrow keys as
   *  `j`/`k` do, and `]p`/`[p` (Task 7) go that many cards on. */
  | { kind: "count"; digit: number }
  /** `[[`/`]]`: the cursor to the previous/next prompt of yours (R4). */
  | { kind: "prompt-jump"; delta: 1 | -1 }
  /** `]p` (+1) / `[p` (-1) (v1 picks, Task 7, ruling R7; nvim's `]d`/`[d` are the model): the cursor to the
   *  next / previous card waiting for an answer in this tab, wrapping where `[[`/`]]` stop; a count repeats
   *  it. It only moves the cursor: `a`/`d` afterwards answer the card it landed on, through S1 as ever. Which
   *  cards wait (not answered from this panel, and the session alive) is `App.tsx`'s call -- it has the
   *  timeline, and this table has none -- so the pair resolves the same on an ended session. */
  | { kind: "card-jump"; delta: 1 | -1 }
  /** `Ctrl+c` while a turn runs (D1/N1/D5): interrupt it. Never claimed idle (native copy) and
   *  never claimed in INPUT (the composer's own `Ctrl+c`, spec §4.1). */
  | { kind: "interrupt" }
  /** `f` in BROWSE: start a global HINT. */
  | { kind: "hint" }
  /** `?` in BROWSE: open or close the full keymap (spec §3). */
  | { kind: "keymap" }
  /** `/` in BROWSE: open R4's incremental search (ruling 25). */
  | { kind: "search" }
  /** `:` in BROWSE (K02, ruling R4 of the v1 picks plan): open a vim-style command line in the
   *  footer that runs no command -- Enter closes it with a flash, Esc silently -- so `:ls⏎`, `:l⏎`
   *  and `:d⏎` land in it and never reach a card. */
  | { kind: "ex-line" }
  /** `n` (+1) / `Shift+N` (-1) in BROWSE: repeat the last `/` search, wrapping (ruling 25). */
  | { kind: "search-next"; delta: 1 | -1 }
  /** `Ctrl+o`, either mode (R3): toggle the detailed view -- every result, wider cuts, no
   *  collapsed runs. */
  | { kind: "detailed" }
  /** `zh` (-1) / `zl` (+1): scroll the current row's own table sideways (T1). */
  | { kind: "table-scroll"; delta: 1 | -1 }
  /** `gf` (N2, ruling 19): open the path(s) on the current row in the editor. */
  | { kind: "open-path" }
  /** `gx` (v1 picks, Task 8, ruling R6; vim's `gx`, "open the URL under the cursor"): open the web link of
   *  the current row in the system browser. `App.tsx` reads the row's links off the DOM (this table has
   *  none): one whose visible text is its own address opens at once, several -- or a titled one, or one
   *  nobody can see -- wait for a letter, each shown in full; none flashes. Like `gf`, an ended session
   *  still has links to open. */
  | { kind: "open-link" }
  /** `Ctrl+g` in BROWSE (R3, ruling 18): view the current row's whole text in an nvim scratch
   *  buffer. Distinct from INPUT's own `Ctrl+g`, which edits the composer's draft (`Composer.tsx`). */
  | { kind: "view-in-editor" }
  /** A `PanelBinding` reached through the panel's own which-key table (`./leader`), rather than a
   *  fixed row above. Reached today only as the second key of a two-key BROWSE prefix (`[b`); the
   *  leader itself and longer sequences run through `./leader`'s `startSequence`/`advanceSequence`,
   *  not this function (panel round 2 plan, Task 7). */
  | { kind: "panel"; binding: PanelBinding }
  /** `v` in BROWSE (spec D2, revised for 3a): start CARET, a moving block caret. `App.tsx` itself
   *  decides whether there is anywhere to start from (a row under the cursor, not a banner or Stop)
   *  -- this table has no DOM to check that with. */
  | { kind: "caret" }
  /** `V` in BROWSE (spec D1, O8's kept default): start V-LINE directly, skipping CARET -- the only
   *  entry route this table still names `"visual"` for; `line` is always `true` here. */
  | { kind: "visual"; line: boolean }
  /** A motion inside CARET/VISUAL/V-LINE (D5): `App.tsx` runs `./visual`'s `stepOnce`/
   *  `repeatMotion` this many times (the count, applied the same way BROWSE's own `j`/`k`/`[[`/`]]`
   *  are). Both ends move in CARET, only `cursor` in VISUAL/V-LINE -- `App.tsx` decides that from
   *  which mode is current, not from this action. */
  | { kind: "vmove"; motion: VisualMotion }
  /** `o` inside VISUAL/V-LINE (D5, `nvim: visual.txt, v_o`): swap `anchor` and `cursor`. Not bound
   *  in CARET (there is only one point to swap). */
  | { kind: "vswap" }
  /** `y` inside VISUAL/V-LINE (D9): copy what is highlighted and end the region, back to BROWSE. */
  | { kind: "vyank" }
  /** `>` inside VISUAL/V-LINE (D10): quote the highlighted text into the tab's draft and end the
   *  region into INPUT. `App.tsx` does the whole thing (the D9 check, `./quote`'s formatting, the
   *  draft/mode/caret side effects) -- this table only names the key. */
  | { kind: "vquote" }
  /** `v`/`V` pressed from CARET, or the OTHER of `v`/`V` pressed from inside VISUAL/V-LINE (D1):
   *  start (from CARET) or switch to (from the sibling mode) VISUAL (`line: false`) or V-LINE
   *  (`line: true`), anchor = cursor = the caret in either case. Never reached for the mode's OWN
   *  key from inside VISUAL/V-LINE -- that is `vback` below. */
  | { kind: "vtoggle"; line: boolean }
  /** `Esc`, or the mode's own `v`/`V`, pressed from inside VISUAL/V-LINE (D1, revised for 3a): back
   *  to CARET, on the moving end -- the region and its freeze stay on. Distinct from `vend`, which
   *  ends the WHOLE region back to BROWSE. */
  | { kind: "vback" }
  /** `Esc`, or any other plain key the current mode does not bind (D12): ends the WHOLE region back
   *  to BROWSE with nothing copied. `key` is `"Escape"` for the former (`App.tsx` shows no flash)
   *  and the key itself for the latter (`App.tsx` builds the "<MODE> ended: <key> is not a <MODE>
   *  key (...)" flash, naming CARET's own hint or VISUAL/V-LINE's). */
  | { kind: "vend"; key: string }
  /** A key carrying Ctrl/Alt/Meta/Super/Hyper/AltGraph inside CARET/VISUAL/V-LINE that this table
   *  does not otherwise claim (D10), or an idle `Ctrl+c` in CARET specifically (D12, a consequence
   *  of D3): swallowed, the mode stays exactly as it was. `App.tsx` flashes "nothing selected — v,
   *  then y" for the CARET-`Ctrl+c` case and says nothing for every other one. */
  | { kind: "vswallow" }
  | null;

/** The nine motions CARET/VISUAL/V-LINE bind (`./visual`'s own table, D5): `h`/`l` by character,
 *  `j`/`k` by screen line, `w`/`e`/`b` by word, `0`/`$` to the hard line's start/end, `gg`/`G` to
 *  the list's first/last selectable character (added for 3a, §9). Declared here, next to the
 *  `PanelAction` variant that carries it, so `resolveKey` and `./visual` share one name for it
 *  rather than each inventing its own union. */
export type VisualMotion = "h" | "l" | "j" | "k" | "w" | "e" | "b" | "0" | "$" | "gg" | "G";

/** The parts of a `KeyboardEvent` this decision needs. A plain object so the table is testable
 *  without a DOM. `keyCode` is read only for C4's legacy-WebKit IME check (some builds report an
 *  IME commit as `keyCode === 229` rather than `isComposing`); absent it is simply not that.
 *  `altKey`/`metaKey` (v1 audit P2-A1): optional, absent meaning "not held" for every existing
 *  caller and test that never mentions them -- the real `KeyboardEvent` App.tsx casts through this
 *  type always carries both at runtime; this type just used to drop them on the floor. Read only by
 *  the card-answer keys (`a`/`d`/`D`, ruling R2): the two-key sequence pair lookup above (`gT` etc.)
 *  is deliberately untouched, a previously accepted exception scoped to that lookup alone (dated
 *  record, 2026-09-27), not to permission answers. (K01 fix round 2: a `FIXED_PAIRS` second key does
 *  not read them either, as the `g`/`z`/`[`/`]` that armed it never did; `resolvePendingSecond`.)
 *  `getModifierState` (fix round 1, v1 audit review, "R2's Super clause"): Super and Hyper are a
 *  THIRD GDK modifier, not a spelling of Meta -- `shell/src/prefix.rs`'s own
 *  `SUPER_MASK | HYPER_MASK | META_MASK` treats all three as distinct, and a first version of this
 *  comment (and of `App.test.tsx`'s R2 test) wrongly claimed "Linux's Super arrives as `metaKey`
 *  too". `KeyboardEvent.getModifierState("Super"/"Hyper")` (the UI Events spec's names for the key)
 *  is the only signal this layer has for it, and the real `KeyboardEvent` App.tsx casts through this
 *  type carries the method natively, so no call site needs to change -- optional here only so a
 *  plain test object that never mentions it keeps meaning "not held". **Not a closed gap**: whether
 *  WebKitGTK's own `WebEventFactory::modifiersForEvent` populates ANY DOM-visible modifier for a
 *  physical Super/Hyper press was not re-checked here (no GUI in this task's scope) -- if it does
 *  not, `getModifierState` is a no-op on that engine too and a real fix needs a shell-side key
 *  controller ahead of the WebView, outside `agent-ui/web`. **Answered (sandbox pass, 2026-09-28):
 *  WebKitGTK 2.52.6 sets neither `metaKey` nor `getModifierState("Super")` while Super is held; its
 *  own keydown/keyup do arrive, so `heldSuper.ts` tracks those and `hasSuperOrHyper` reads it too.**
 *  Fix round 2 reads `getModifierState`
 *  once more, for `"AltGraph"` (`hasAltGraph`) -- the identical caveat applies to it: not re-checked
 *  against real WebKitGTK either. */
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

/** Fix round 1 (v1 audit review, "R2's Super clause"): whether Super or Hyper is held, read the only
 *  way this layer can (see `KeyLike`'s own doc comment on `getModifierState` for the honest caveat
 *  about whether WebKitGTK ever actually sets either). */
function hasSuperOrHyper(event: KeyLike): boolean {
  // WebKitGTK 2.52.6 reports neither on another key's event (sandbox pass, 2026-09-28), so the Super
  // key's own keydown/keyup, tracked by `heldSuper.ts`, is what actually catches it there.
  return (
    superHeld() || event.getModifierState?.("Super") === true || event.getModifierState?.("Hyper") === true
  );
}

/** Fix round 2 (v1 audit review, "the AltGraph clause"): whether AltGr is held, read the same way
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
 *  this, the card-answer switch below (`a`/`d`/`D`, R2 and its Super follow-up) and `App.tsx`'s
 *  bypass-confirm `y` handler (325e007, a whole-branch codex review on `feat/v1-dist--D`) each
 *  hand-wrote their own version of this list; 325e007's never checked Super/Hyper at all, which is
 *  exactly the kind of drift two independent lists invite. Composing is folded in here too,
 *  duplicating `composerKeys.ts`'s `isImeKey` rather than importing it, the same reason
 *  `resolveKey`'s own top-of-function check does (`KeyLike` stays a plain object with no dependency
 *  of its own). AltGraph joined this list in fix round 2, closing a gap neither surface had checked
 *  (a same-day review, run after the reconciliation above already shipped). Shift is deliberately
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
  /** The previous key was the first of a two-key BROWSE sequence and nothing has come since. Held
   *  by the caller (`App.tsx`), never by this table, so `resolveKey` stays a function of its three
   *  arguments. The caller clears it on every key; outside INPUT, any key but a bare modifier, a
   *  matching second half or `Ctrl+c` (D1) resolves to `{kind: "cancel"}` (K01), never to its own
   *  meaning -- chords included (`resolvePendingSecond`). INPUT ignores it. */
  pending?: PendingPrefix | null;
  /** The count `App.tsx` has accumulated from `1`-`9` then `0`-`9` (R4), or `null`/absent before any
   *  digit has been pressed. This table only ever reads it to decide whether a lone `0` starts a
   *  count (it does not) or continues one (it does); applying the count to `j`/`k`/`[[`/`]]`, to
   *  `G`/`gg`/`zt`/`zz`/`zb` (row N), to `gt`/`gT` (tab N) and to `]p`/`[p` (that many cards on) is
   *  `App.tsx`'s job, since it is the one that knows how many times to repeat a move, how many rows there
   *  are, which tabs and which cards wait. */
  count?: number | null;
  /** Whether a turn is running in the active tab, for `Ctrl+c` (D1/N1). `undefined`/`false` both
   *  mean "not running" -- idle is the common case and every existing caller that never mentions
   *  this field must keep meaning idle. */
  turnRunning?: boolean;
  /** The panel's own which-key table (panel round 2 plan, Task 7), absent before the first `keymap`
   *  envelope. Consulted only for a second key completing a two-key BROWSE prefix (`[b`); `App.tsx`
   *  is expected to pass `KeymapHelp.panel` (or leave it unset), never `EMPTY_PANEL_TABLE` -- an
   *  empty table and an absent one behave identically here (no bindings to match). */
  table?: PanelTable;
};

/** D5's `gg`/`G`, and D2's `gv` reservation, shared by CARET and VISUAL/V-LINE (both bind `gg`/`G`
 *  the same way, through a region-local pending `g` -- distinct from BROWSE's own `ctx.pending`,
 *  which this only reuses as a carrier, never as BROWSE's actual two-key prefix state). Returns a
 *  `PanelAction` when the pending-`g` case is fully handled (the pending marker itself, the `gg`
 *  motion, or `gv`'s own "nothing"); `"fallthrough"` means "not a pending-`g` case, keep resolving
 *  this key normally" -- which is how `g` followed by any other key ends up handled on its own
 *  (D5's own text: "`g` then any key but `g` drops the `g` and handles that key alone"). `G`
 *  (Shift+`g`) never reaches this at all: it is a single keystroke, resolved as its own motion in
 *  each caller's switch below. */
function resolveRegionPendingG(event: KeyLike, ctx: KeyContext): PanelAction | "fallthrough" {
  if (ctx.pending === "g") {
    if (event.key === "g") return { kind: "vmove", motion: "gg" };
    // D2: `gv` is reserved for vim's reselect, even unbuilt -- swallowed as "nothing" rather than
    // falling through to `vtoggle`'s own `v` handling.
    if (event.key === "v") return null;
    return "fallthrough";
  }
  if (event.key === "g") return { kind: "pending", prefix: "g" };
  return "fallthrough";
}

/** The Ctrl/Alt/Meta/Super/Hyper/AltGraph checks D10/D12 give CARET and VISUAL/V-LINE in common:
 *  `Ctrl+c` keeps interrupting a running turn in every mode, and any other held modifier is
 *  swallowed. `idleCtrlC` is the one place the two callers genuinely differ (D12: claimed in
 *  CARET, left to the browser's native copy in VISUAL/V-LINE) -- passed in rather than branched on
 *  `mode` here, so this function stays a pure function of the event and that one flag.
 *
 *  v1 trial seam review finding 3 (2026-09-28): Ctrl+e/Ctrl+y used to fall straight through to the
 *  generic swallow just below, with no feedback at all -- even though the `?` overlay's own note
 *  said "Any other key leaves" and CARET_KEYS' `?` row said "any other key ends the caret too".
 *  vim scrolls on these keys in Visual mode (`:help CTRL-E`/`:help CTRL-Y`); this table now does
 *  too, reusing BROWSE's own `scroll-line` action (`resolveKey`'s `mode !== "input"` block below,
 *  which CARET/VISUAL/V-LINE never reach -- they return from `resolveCaretKey`/`resolveVisualKey`
 *  before that point) rather than a second copy of the chord check. `App.tsx`'s region switch scrolls
 *  the frozen list one line per press without moving the caret/selection or ending the region --
 *  D10's own list of what the region swallows is otherwise unchanged. */
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
 * CARET's own key table (spec §1/§9, D2-D5/D10/D12): a block caret, moved by the same motions
 * VISUAL/V-LINE use, that starts VISUAL/V-LINE rather than being either of them. Called by
 * `resolveKey` for `mode === "caret"`, never directly: every CARET key is decided here, with no
 * fallthrough to BROWSE/INPUT or to `resolveVisualKey` below it.
 */
function resolveCaretKey(event: KeyLike, ctx: KeyContext): PanelAction {
  // D12 (3a): idle `Ctrl+c` is CLAIMED in CARET (D3's own consequence: the engine would otherwise
  // copy the one character the caret sits on) -- `App.tsx` reads `vswallow` plus `mode === "caret"`
  // to flash "nothing selected — v, then y", rather than a dedicated action kind, per spec §8.
  const modifierResult = resolveRegionModifierKey(event, ctx, { kind: "vswallow" });
  if (modifierResult !== "fallthrough") return modifierResult;
  // D12: `?` ends CARET and opens the full keymap.
  if (event.key === "?") return { kind: "keymap" };
  const pendingResult = resolveRegionPendingG(event, ctx);
  if (pendingResult !== "fallthrough") return pendingResult;
  // R4/D5: `1`-`9` starts a count, `0`-`9` continues one; a LONE `0` (no count running) is the `0`
  // motion instead of a digit, since CARET binds `0` to a real motion where BROWSE does not.
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
      // D1: CARET's `v`/`V` always START a selection mode, anchor = cursor = the caret -- never
      // "ends" the way VISUAL/V-LINE's OWN key does (that comparison does not exist in CARET).
      return { kind: "vtoggle", line: false };
    case "V":
      return { kind: "vtoggle", line: true };
    case "Escape":
      // D1: CARET's `Esc` ends the WHOLE region, back to BROWSE on the caret's row -- `App.tsx`
      // tells this apart from an ordinary unbound key by `action.key === "Escape"`.
      return { kind: "vend", key: "Escape" };
    default:
      // D12: any other plain key CARET does not bind ends the region, clears it, and does nothing
      // else -- never its BROWSE meaning (`a`/`d`/`D`/`i`/`o`/Enter/Space included).
      return { kind: "vend", key: event.key };
  }
}

/**
 * VISUAL/V-LINE's own key table (spec §1/§9, D4/D5/D9/D10/D12). Called by `resolveKey` for either
 * mode, never directly: every VISUAL/V-LINE key is decided here, with no fallthrough to the
 * BROWSE/INPUT logic below it. `mode` tells the mode's OWN `v`/`V` (D1: back to CARET, `vback`)
 * apart from the OTHER one (switch to the sibling selection mode, `vtoggle`) -- `App.tsx` still
 * reads `vswap`/`vmove` off nothing but the action itself, since neither depends on which of the
 * two this is.
 */
function resolveVisualKey(mode: "visual" | "vline", event: KeyLike, ctx: KeyContext): PanelAction {
  // D10: `Ctrl+c` keeps its BROWSE meaning in VISUAL/V-LINE -- idle, unclaimed (the engine's own
  // native copy of the visible selection runs and the mode stays, exactly as an unclaimed key
  // always leaves it). Bare modifiers (Shift on the way to `$`/`V`) are exempted the same way.
  const modifierResult = resolveRegionModifierKey(event, ctx, null);
  if (modifierResult !== "fallthrough") return modifierResult;
  // D10: `?` ends VISUAL/V-LINE and opens the full keymap -- `App.tsx` ends the region itself on
  // this action, the same as every other route into the overlay.
  if (event.key === "?") return { kind: "keymap" };
  const pendingResult = resolveRegionPendingG(event, ctx);
  if (pendingResult !== "fallthrough") return pendingResult;
  // R4/D5: `1`-`9` starts a count, `0`-`9` continues one; a LONE `0` (no count running) is the `0`
  // motion instead of a digit, since VISUAL binds `0` to a real motion where BROWSE does not.
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
      // D1: VISUAL's OWN key (`v`) goes back to CARET; V-LINE's OTHER key switches to VISUAL.
      return mode === "visual" ? { kind: "vback" } : { kind: "vtoggle", line: false };
    case "V":
      // D1: V-LINE's OWN key (`V`) goes back to CARET; VISUAL's OTHER key switches to V-LINE.
      return mode === "vline" ? { kind: "vback" } : { kind: "vtoggle", line: true };
    case "Escape":
      // D1: `Esc` from VISUAL/V-LINE goes back to CARET (not all the way to BROWSE) -- the region
      // and its freeze stay on (D13/D14: "Esc from VISUAL is not an exit").
      return { kind: "vback" };
    default:
      // D12: any other plain key VISUAL/V-LINE does not bind ends the WHOLE region, clears it, and
      // does nothing else -- never its BROWSE meaning (`a`/`d`/`D`/Enter/`g`/Space included).
      return { kind: "vend", key: event.key };
  }
}

/** K01 (X-A-11): the keys that complete each reserved BROWSE prefix -- vim waits for a prefix's
 *  second key with no timeout, and a key it does not know ends the command (`nv_g_cmd`, `nv_zet`,
 *  `nv_brackets`: `clearopbeep`). Anything else cancels (`resolvePendingSecond`): it never falls
 *  through to its own meaning, so `g`, a pause, `d` cannot deny a card. Table pairs (`[b`, `gt`,
 *  `gT`) are looked up first, in `resolveKey`. Later keys add pairs here, never a fall-through --
 *  and each pair a row in `leader.ts`'s `FIXED_PENDING_ENTRIES`, the which-key box drawn after its
 *  prefix (`leader.test.ts` holds the two to the same keys). */
export const FIXED_PAIRS: Record<PendingPrefix, Readonly<Record<string, NonNullable<PanelAction>>>> = {
  // v1 picks, Task 8 (R6): `gx`, vim's "open the link", beside `gf`'s "open the path".
  g: { g: { kind: "jump", to: "first" }, f: { kind: "open-path" }, x: { kind: "open-link" } },
  z: {
    h: { kind: "table-scroll", delta: -1 },
    l: { kind: "table-scroll", delta: 1 },
    // v1 picks, Task 4: vim's `zt`/`zz`/`zb` (the row to the top/middle/bottom of the view) and
    // `za`/`zo`/`zc` (toggle/open/close a fold; `za` is Enter's own `toggle-expand`).
    t: { kind: "scroll-row", where: "top" },
    z: { kind: "scroll-row", where: "center" },
    b: { kind: "scroll-row", where: "bottom" },
    a: { kind: "toggle-expand" },
    o: { kind: "fold", open: true },
    c: { kind: "fold", open: false },
  },
  // v1 picks, Task 7 (R7): `[p`/`]p`, the previous/next card waiting for an answer, beside the prompt pair.
  "[": { "[": { kind: "prompt-jump", delta: -1 }, p: { kind: "card-jump", delta: -1 } },
  "]": { "]": { kind: "prompt-jump", delta: 1 }, p: { kind: "card-jump", delta: 1 } },
  // v1 picks, Task 6 (R11): vim's `CTRL-W h/j/k/l`, the module that way. `Ctrl+w w/p/o/q` and `Ctrl+w
  // Ctrl+h` are not here: they cancel, like every other key after this prefix.
  "C-w": {
    h: { kind: "pane", direction: "left" },
    j: { kind: "pane", direction: "down" },
    k: { kind: "pane", direction: "up" },
    l: { kind: "pane", direction: "right" },
  },
};

/** The key after a pending prefix (K01). A bare modifier is not a key yet. `Ctrl+c` keeps D1's
 *  meaning; `gv` stays D2's reservation and `[]`/`][` stay nothing, exactly as before.
 *  Fix round 2 (review): the second key reads the modifiers the way `[`/`]` themselves are read
 *  (`resolveKey`'s blanket refusal: Ctrl and Shift, never Alt or Meta), so the two halves of a
 *  bracket pair agree. A pair typed with Alt or Meta held completes, as it did before K01 -- on a
 *  layout that types a bracket with Option (macOS German, French, Swiss), that is how `[[` and `]]`
 *  are typed at all -- and a key completing no pair still cancels, whatever it holds.
 *  rc.3 minors 6: `g` and `z` no longer ARM with Alt or Meta held (no layout types them that way),
 *  so for them only the second key still reads Alt/Meta -- a pair whose first half was plain. */
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
  // CARET/VISUAL/V-LINE (spec §2, revised for 3a §9, D2-D5/D9/D10/D12): a whole branch of its own,
  // right after the IME check and ahead of `Ctrl+o` just below -- `Ctrl+o` must NOT toggle the
  // detailed view while the region is on (D10/D12: it is one of the chords the region swallows
  // rather than lets through to its BROWSE meaning). `resolveCaretKey`/`resolveVisualKey` never
  // fall through to anything below them; every region key is decided in one of the two or nowhere.
  if (mode === "caret") return resolveCaretKey(event, ctx);
  if (mode === "visual" || mode === "vline") return resolveVisualKey(mode, event, ctx);
  // The key after a pending prefix, outside INPUT (INPUT never reads one: a `g` left over from
  // BROWSE is simply dropped there). First, a second key completing a panel-table two-key sequence
  // whose first half is one of the four prefixes (`[b`, spec §2.3 -- a prefix listed there may start
  // a longer sequence than `FIXED_PAIRS` knows about), which applies to a second key typed with
  // Shift held too: vim's `gT` (`:help gT`, v1 polish F16) arrives as `key: "T"` with `shiftKey`.
  // Ctrl never completes a pair -- no table key is a Ctrl chord. Then K01 (ruling R1): whatever else
  // follows completes one of the fixed pairs or cancels. Both run here, ahead of every chord below
  // (`Ctrl+o` included: matched first, `g` then Ctrl+o toggled the detailed view -- K01 fix round 1)
  // and of the blanket modifier refusal, so no key ever runs as itself after a prefix.
  if (mode !== "input" && ctx.pending) {
    if (!event.ctrlKey) {
      const tableHit = ctx.table?.bindings.find((b) => b.keys.length === 2 && b.keys[0] === ctx.pending && b.keys[1] === event.key);
      if (tableHit) return { kind: "panel", binding: tableHit };
    }
    return resolvePendingSecond(ctx.pending, event, ctx);
  }
  // R3: `Ctrl+o` toggles the detailed view in EITHER mode -- named here, ahead of the `mode !==
  // "input"` block below (whose chords are BROWSE/HINT-only) and ahead of the blanket modifier
  // refusal further down, the same reason `?`/`G`/`N` are. It works while typing (spec: "it works
  // while typing too") because a reader deciding whether a fold is hiding something they need does
  // not want to leave the box first.
  if (event.ctrlKey && !event.shiftKey && event.key === "o") return { kind: "detailed" };
  // The chords this table names outside INPUT, checked BEFORE the blanket modifier refusal below and
  // before the plain-key switch. The order is load-bearing: a browser reports `key === "d"` for
  // Ctrl+d just as for a bare `d` (Ctrl does not remap `key` the way Shift does), so a Ctrl+d that
  // reached the switch would land on `case "d"` and DENY a pending permission. Each chord is matched
  // on its exact modifier set -- Ctrl+Shift+d is none of them and is refused below like any other
  // chord. GTK claims none of these before the WebView (checked against `shell/src/main.rs` and
  // `shell/src/agent_panel.rs`, 2026-09-19): only a user's own Lua `keybinding` could.
  if (mode !== "input") {
    if (event.ctrlKey && !event.shiftKey && (event.key === "d" || event.key === "u")) {
      return { kind: "half-page", delta: event.key === "d" ? 1 : -1 };
    }
    // v1 trial item 5: vim's own `Ctrl+e`/`Ctrl+y`, checked here for the same reason as Ctrl+d/
    // Ctrl+u just above -- `key` reports "e"/"y" the same under Ctrl as bare, and bare `y` (the
    // plain-key switch below) copies the current row, so an unchecked Ctrl+y would copy instead
    // of scrolling. INPUT's own `Ctrl+e` (end of line) is untouched: this whole block is gated on
    // `mode !== "input"` already.
    if (event.ctrlKey && !event.shiftKey && (event.key === "e" || event.key === "y")) {
      return { kind: "scroll-line", delta: event.key === "e" ? 1 : -1 };
    }
    // v1 picks, Task 5 (decision #13, ruling R10): vim's `Ctrl+f`, a view down, named here for the same
    // reason as `Ctrl+d`/`Ctrl+e` -- `key` is "f" under Ctrl as bare, and a bare `f` starts a HINT (the
    // plain-key switch below). Its way back is `PageUp`: `Ctrl+b` is shell's tmux prefix by default,
    // taken by GTK before the page sees it, and stays unclaimed here. INPUT is untouched (this whole
    // block is `mode !== "input"`).
    if (event.ctrlKey && !event.shiftKey && event.key === "f") return { kind: "page", delta: 1 };
    // v1 picks, Task 6 (R11): `Ctrl+w`, vim's window prefix, waits for `h`/`j`/`k`/`l` (`FIXED_PAIRS["C-w"]`)
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
    // D1/N1/D5: Claude Code's `Ctrl+c` interrupt, minus the exit half -- this table never closes
    // anything. Idle, it is left to the browser (a text selection's native copy); `Ctrl+c` in INPUT
    // is the composer's own (spec §4.1, ruling 31), so it is deliberately not named here at all.
    if (event.ctrlKey && !event.shiftKey && event.key === "c") return ctx.turnRunning === true ? { kind: "interrupt" } : null;
    if (event.shiftKey && !event.ctrlKey && event.key === "G") return { kind: "jump", to: "last" };
    // `?` arrives with Shift held on most layouts (Shift+/), so it must be named before the blanket
    // modifier refusal below, the same reason `G` is; matched on `key`, never on the physical key.
    if (event.key === "?" && !event.ctrlKey) return { kind: "keymap" };
    // K02 (ruling R4): `:` opens the command line, named here for the same reason as `?` -- Shift+;
    // on most layouts, unshifted on others (AZERTY). Held with Ctrl, Alt or Meta it is some other
    // chord and falls through. After a pending prefix it never gets here (K01 cancels it above).
    if (event.key === ":" && !event.ctrlKey && !event.altKey && !event.metaKey) return { kind: "ex-line" };
    // R4: `Shift+N` repeats the last `/` search backward, the same reason `G` is checked here --
    // most layouts deliver capital `N` with Shift held.
    if (event.shiftKey && !event.ctrlKey && event.key === "N") return { kind: "search-next", delta: -1 };
    // N3/P5: `Shift+Y`/`Shift+D`, named here ahead of the blanket modifier refusal for the same
    // reason as `G`/`N`/`?` -- most layouts deliver both with Shift held. `D` needs a live session
    // to have anywhere to put the keys (its own row is refused the same way `i`'s and `r`'s are).
    if (event.shiftKey && !event.ctrlKey && event.key === "Y") return { kind: "copy-output" };
    // R2 (v1 audit P2-A1): Alt/Meta/Super held alongside Shift+D must not reach a card-answer key
    // -- checked here, ahead of the blanket ctrl/shift-only refusal below, since that refusal never
    // looks at altKey/metaKey (or Super/Hyper/AltGraph) at all. `isPlainAnswerKey` (v1 audit fixes,
    // 2026-09-28) is what actually reads all of Ctrl/Alt/Meta/Super/Hyper/AltGraph here -- there is
    // no separate `!event.ctrlKey` in this condition any more, only the predicate.
    if (event.shiftKey && isPlainAnswerKey(event) && event.key === "D") {
      // K01 (ruling R2): a card answer takes no count -- `3D` is refused and says so, never answered.
      if ((ctx.count ?? null) !== null) return { kind: "cancel", why: "count-on-answer" };
      return ctx.sessionEnded ? null : { kind: "deny-reason" };
    }
    // C1a: `A` (`:h A`), named ahead of the blanket refusal the same reason G/N/Y/D are -- most
    // layouts deliver it with Shift held. Refused on an ended session for the same reason `i`/`o`
    // are just below: INPUT there has no box to place a caret in (spec §3.2).
    if (event.shiftKey && !event.ctrlKey && event.key === "A") {
      return ctx.sessionEnded ? null : { kind: "mode", to: "input", caret: "end" };
    }
    // R3: `Ctrl+g` in BROWSE views the current row in nvim. INPUT's own `Ctrl+g` (the composer's
    // edit-in-nvim) is a different action entirely, which is exactly why this is gated on `mode !==
    // "input"` rather than named unconditionally -- it must never shadow the composer's chord.
    if (event.ctrlKey && !event.shiftKey && event.key === "g") return { kind: "view-in-editor" };
    // (D2's `gv` reservation -- vim's reselect, `nvim: visual.txt, gv`, still unbuilt -- lives in
    // `resolvePendingSecond` now, with every other key a pending prefix can take: K01 returns from
    // that function above this block for any pending prefix, so a leftover `g` can never reach the
    // plain `v` entry just below and start CARET. A user's own `gv` panel binding still wins,
    // through the table lookup just ahead of it.)
    // D2 (revised for 3a, §9): BROWSE's `v` starts CARET (was `{kind: "visual", line: false}` in
    // the first design); `V` still starts V-LINE directly (O8's kept default). Named here ahead of
    // the blanket modifier refusal below the same reason `G`/`N`/`Y`/`D`/`A` are -- `V` carries
    // Shift on most layouts. A configured leader or panel binding on `v`/`V` wins over this: both
    // reach here only once the leader engine (`App.tsx`, ahead of every call to this function) has
    // already passed on the key. Fix round 2 (reviewer finding, minor): `isPlainAnswerKey`, the
    // same predicate the sibling `D` row above reads, not a hand-written `!ctrlKey` -- that let
    // Alt+v, Meta+v, Super+v and AltGr+v start a mode. Each of those falls through unclaimed now,
    // exactly what it did before the region existed (the switch below has no `v`).
    if (isPlainAnswerKey(event) && !event.shiftKey && event.key === "v") return { kind: "caret" };
    if (isPlainAnswerKey(event) && event.shiftKey && event.key === "V") return { kind: "visual", line: true };
  }
  // A key carrying a modifier this table does not name is not claimed. Apart from the chords just
  // above, no row needs Ctrl or Shift, so any other chord holding either falls through unclaimed --
  // an ordinary Shift+letter, and any Ctrl+letter GTK or a future binding wants, both included.
  // `KeyLike` has carried these two fields since the table was first written; this is what makes
  // reading them, rather than deleting them, the correct fix once something actually checked whether
  // they were used.
  // Owner decision #39 (2026-09-30, "input 直接ctrl y统一吧，不用两次"): INPUT's one added chord, ahead
  // of the blanket refusal just below. Exactly Ctrl+y -- Shift, Alt, Meta, Super/Hyper (held on the
  // event or by its own keydown, `hasSuperOrHyper`) or AltGraph held is nothing, the same modifier set
  // `isPlainAnswerKey` refuses for `a`/`d`, with Ctrl required instead of refused; a composing input
  // method already returned at the top of this function. `App.tsx` picks the card (the active tab's
  // oldest waiting one) and runs S1's guard; this table only names the chord. BROWSE's own Ctrl+y
  // (the one-line scroll) is the `mode !== "input"` block above and never reaches here. Fix round 1
  // (Opus I-4): under Caps Lock GTK/WebKitGTK report the chord as key "Y" with Shift not held, so the
  // letter is compared case-blind -- Shift itself is still refused just below.
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
    // An Esc mid-composition belongs to the input method -- fcitx uses it to cancel the preedit.
    // Eating it breaks pinyin, which this project verified end to end (P4) and must not regress.
    // (The top-of-function IME check above already returns for that case; this `!event.isComposing`
    // is the older, narrower guard it superseded and is kept for exactly this one branch's clarity.)
    if (event.key === "Escape" && !event.isComposing) return { kind: "mode", to: "browse" };
    return null;
  }
  // (The second key of a two-key sequence -- a table pair such as `[b`/`gt`/`gT`, or one of
  // `FIXED_PAIRS`' own: `gg`, `gf` (N2, vim's "go to file"), `gx` (Task 8, R6: open the row's web link),
  // `zh`/`zl` (T1, a row's table
  // sideways), `zt`/`zz`/`zb` and `za`/`zo`/`zc` (v1 picks, Task 4: the row to an edge of the view,
  // a fold toggled/opened/closed), `[[`/`]]` and `[p`/`]p` (Task 7: the previous/next prompt, the
  // previous/next card waiting) -- is resolved above, ahead of `Ctrl+o`, the other
  // chords and the blanket modifier refusal. Since K01 nothing outside INPUT that follows a pending
  // prefix reaches this point: a key completing no pair cancels there (`resolvePendingSecond`), as
  // vim ends an unfinished `g`, rather than falling through here as an ordinary key. A mismatched
  // pair (`[` then `]`) is still claimed as nothing, so it cannot be misread as the OTHER prefix's
  // first half.)
  // R4: a count for `j`/`k`/`[[`/`]]`. `1`-`9` starts one; `0` only continues one already started --
  // a lone `0` is an ordinary (unclaimed) key, since nothing in BROWSE starts with `0`.
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
      // because giving `o` a newline meaning later would change what it does today (spec §3.2).
      return ctx.sessionEnded ? null : { kind: "mode", to: "input", caret: "kept" };
    // v1 picks, Task 5 (ruling R10): the arrow keys are exactly `j`/`k` -- the same case, so the two
    // can never drift apart (a count, the walk through a tall row, the S1 guard are all `move`'s).
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
    // v1 picks, Task 5 (decision #13): PageDown is `Ctrl+f`'s twin (named with the chords above),
    // PageUp the way back a page. Shift or Ctrl held never gets here: the blanket refusal above.
    case "PageDown":
      return { kind: "page", delta: 1 };
    case "PageUp":
      return { kind: "page", delta: -1 };
    case "a":
    case "d":
      // R2 (v1 audit P2-A1): Alt+a/Meta+a must not authorize or deny a card -- a common OS chord
      // (e.g. "select all" muscle memory) held with `a` used to reach here unchecked, since the
      // blanket ctrl/shift-only refusal above never looks at altKey/metaKey and this switch is keyed
      // on `event.key` alone.
      // Fix round 1 (v1 audit review, "R2's Super clause"): Super/Hyper is a THIRD, separate GDK
      // modifier -- it does not "arrive as metaKey", the claim an earlier version of this comment
      // made and this project's own shell-side code (`SUPER_MASK`/`HYPER_MASK`/`META_MASK` as three
      // distinct masks, `shell/src/prefix.rs`) disproves. `isPlainAnswerKey` is the shared check for
      // this (v1 audit fixes, 2026-09-28); see `KeyLike`'s own doc comment for the honest caveat that
      // Super may be a no-op on WebKitGTK if it never surfaces as a DOM modifier at all, in which case
      // a real fix needs a shell-side key controller (out of this task's touches list; not built here).
      if (!isPlainAnswerKey(event)) return null;
      // K01 (ruling R2): a card answer takes no count -- `3a` is refused and says so, never answered.
      if ((ctx.count ?? null) !== null) return { kind: "cancel", why: "count-on-answer" };
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
    case "/":
      return { kind: "search" };
    case "n":
      return { kind: "search-next", delta: 1 };
    case "Escape":
      // R34: `Esc` never interrupts (D1) -- while a reply runs, this says so rather than silently
      // doing nothing; idle, it is unclaimed exactly as before.
      return ctx.turnRunning === true ? { kind: "esc-blocked" } : null;
    default:
      return null;
  }
}

/** One line of the `?` keymap: the key as a person types it, and what it does. */
export type KeyHelp = { keys: string; what: string };

/** The PageUp row's text under the effective prefix (`KeymapHelp.prefix`, as a person reads it). vim's way
 *  back a page is `Ctrl+b`, which this panel never claims (`resolveKey`'s `Ctrl+f` note); the row says why
 *  only when `Ctrl+b` really is the prefix -- under another one (the owner's `Ctrl+a`) naming `Ctrl+b` as
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
 *  leave out one that does (spec §3.3). */
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
  { keys: "D", what: "Deny with a reason: into the card's reason box, Enter denies" },
  { keys: "i / o", what: "Start typing, caret where you left it (C1a: o is an exact alias of i)" },
  { keys: "A", what: "Start typing at the end of the draft" },
  { keys: "f", what: "HINT: jump anywhere in the window (links too; Enter opens one)" },
  { keys: "r", what: "New session, once this one has ended" },
  { keys: "/", what: "Search the conversation (Enter keeps the match, Esc goes back)" },
  { keys: ":", what: "A command line, as in vim: nothing runs here yet; Enter or Esc closes it" },
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

/** CARET, i.e. everything `resolveCaretKey` claims (spec §1/§9, D2-D5/D12). Tied to `resolveKey`'s
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

/** VISUAL/V-LINE, i.e. everything `resolveVisualKey` claims (spec §1/§9, D4/D5/D9/D10). Tied to
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
  // Owner decision #27 (K14): Claude Code's own two newline keys, which used to send the draft.
  { keys: "Alt+Enter / \\ Enter", what: "New line (a backslash right before the caret is replaced by it)" },
  { keys: "↑ / ↓", what: "From the first / last line: the queue back, then earlier prompts" },
  { keys: "Ctrl+r", what: "Search earlier prompts (Enter puts one in the box)" },
  { keys: "Ctrl+w / Ctrl+u", what: "Delete a word / to the line's start" },
  { keys: "Ctrl+c", what: "Interrupt a running turn; idle, clear the box into history" },
  // Owner decision #39 (2026-09-30): an added key. The band names the card it would approve.
  { keys: "Ctrl+y", what: "Approve the oldest card waiting in this tab (the band names it); only a lone Ctrl+y answers" },
  { keys: "Ctrl+g", what: "Edit this in nvim (:wq brings it back, :q! changes nothing)" },
  { keys: "Ctrl+o", what: "Detailed view" },
  { keys: "Shift+Tab", what: "Toggle auto ⇄ bypass (entering bypass asks first)" },
  { keys: "Esc", what: "Stop typing (back to browsing)" },
  { keys: "?", what: "This list, from an empty box" },
];

/** `shell`'s own keys, which nothing on this page can read: sent by `shell` in a `keymap` envelope
 *  on every `ready`, generated from `eitri_core::keymap` (the root table and the effective prefix
 *  table after `init.lua`). `prefix` is the prefix as a person reads it (`Ctrl+b`).
 *
 *  `panel` and `newTabChord` are the panel round 2 plan's own additions (Task 5/6):
 *  `serialize_keymap_for_js` now also carries this panel's own which-key table (`panel`, spec
 *  §10.1, `defaults < nvim < init.lua`'s `effective()`) and the prefix chord that opens a new tab,
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
