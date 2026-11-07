/** What the band says when a typed `a`/`d`/`D`, or an Enter on a card button,
 *  did not answer because another key came within `TYPING_GUARD_MS` of it (`./typingGuard`). It
 *  says to pause, since `Esc` then `a` -- the route the activity line names --
 *  is refused the same way when the two come close together, and "type" alone read as if the user
 *  had been typing. */
export const TYPING_FLASH = "a / d answer a card only on their own — pause, then press again; i or Ctrl+j to type";
/** What the band says when `Esc` is pressed while a turn runs: it never interrupts, and this is the reminder. */
export const ESC_DOES_NOT_INTERRUPT_FLASH = "Esc does not interrupt — ctrl+c does";

/** The short form the band shows of a flash too wide for its one row (`bandLayout`), tried once every segment that
 *  can go has gone and before the mode shrinks to its letter: a narrow chat (a column of a split window, a laptop at
 *  zoom 1.5) keeps one row and its composer uncovered, and still says what to do. A flash without a short form, or
 *  whose short form does not fit either, wraps as before. */
export const SHORT_FLASH: ReadonlyMap<string, string> = new Map([
  [TYPING_FLASH, "pause, press again"],
  [ESC_DOES_NOT_INTERRUPT_FLASH, "ctrl+c interrupts"],
]);
/** What the band says when `a`/`d`/`D`, or Enter/Space on a card's own button,
 *  came after a count (`3a`) -- a card answer takes no count, so the key answers nothing. */
export const COUNT_ANSWER_FLASH = "a / d / D take no count — press it on its own";
/** What the band says when Enter on a card's own button came with a modifier held
 *  -- Shift included, which `isPlainAnswerKey` lets through for `D` -- and answered nothing. */
export const MODIFIED_ENTER_FLASH = "Enter with a modifier answers no card";
/** What the band says when Enter on a card's own button came with no landing right
 *  before it -- focus put on that button by `l`/`h`, a Tab or a HINT -- and answered nothing. Before
 *  that check, `l`, a pause, `s`, Enter (`:ls⏎`) approved the card natively. */
export const ENTER_LANDING_FLASH = "Enter answers a card right after l, h or Tab onto its button";
/** What the band says when Enter on a card's own button, landed on, is on a
 *  card other than the one `a`/`d` would answer from the cursor (`permissionTarget`) -- a Tab walks on
 *  from one card's buttons into the next card's, and `l`/`h` then walk that card's, while the row
 *  cursor stays where it was. */
export const ENTER_ELSEWHERE_FLASH = "Enter answers the card under the cursor — j / k onto it first";
/** What the band says when `a`/`d`/`D` have no card under the cursor, nor a
 *  card gating the tool call under it, and no card waits anywhere -- instead of doing nothing
 *  silently. */
export const NO_CARD_FLASH = "no card here — i, o, A or Ctrl+j to type";
/** The same refusal while a card IS waiting, just not under the cursor -- it
 *  points back to the card, as the activity line's "j to it" does, instead of only suggesting typing. */
export const CARD_ELSEWHERE_FLASH = "a / d answer the card under the cursor — j / k onto it, then a / d";
/** What the band says when `]p` / `[p` found no card waiting for an answer in this
 *  tab -- none at all, every one already answered from this panel, or a session that ended (its cards are
 *  inert) -- instead of doing nothing silently. */
export const NO_WAITING_CARD_FLASH = "no card waiting here";
/** What the band says when Enter or Space on the activity line's
 *  Stop did not interrupt because it came in the middle of typing (`TypingGuard.mayActAfterMotion`)
 *  -- "just do it" typed after an arrival reached Stop with its `j` and interrupted the turn. */
export const STOP_TYPING_FLASH = "Stop takes a key only on its own — i or Ctrl+j to type";
/** What the band says when INPUT's `Ctrl+y` came within `TYPING_GUARD_MS` of
 *  another key (readline's `Ctrl+u`/`Ctrl+w` then `Ctrl+y` yank, typing right after it, a held
 *  key's repeat) and approved nothing. */
export const CTRL_Y_TYPING_FLASH = "Ctrl+y approves only on its own — pause, then press it again";
/** What the band says when INPUT's `Ctrl+y` came less than
 *  `TYPING_GUARD_MS` after the card it would approve became the one it names -- a card that just
 *  arrived, one that replaced a withdrawn card, or the next card right after an approval. */
export const CTRL_Y_TARGET_FLASH = "Ctrl+y: the waiting card just changed — read it, then press again";
/** What the band says when INPUT's `Ctrl+y` came right after a kill in the
 *  box (`Ctrl+u`, `Ctrl+w`, `Ctrl+k`), however long the pause -- readline's yank, never an answer. */
export const CTRL_Y_YANK_FLASH = "Ctrl+y right after Ctrl+u, Ctrl+w or Ctrl+k is a yank, not an answer — press it again";
/** What the band says when INPUT's `Ctrl+y` was let go more than `RELEASE_WAIT_MAX_MS` after it was
 *  pressed: a held key, which approves nothing even where no repeat came to refuse it. */
export const CTRL_Y_HELD_FLASH = "Ctrl+y held down approves nothing — tap it to approve";
/** What the band says when INPUT's `Ctrl+y` is pressed with only a card of Eitri's own tool broker waiting: such a
 *  card is answered by a key on its own row, never by a key that names no card. */
export const CTRL_Y_EDITOR_CARD_FLASH = "Ctrl+y does not answer an editor card: Esc, then a or d on the card";
/** What the band says when `a` or INPUT's `Ctrl+y` would approve a card whose Approve is disabled: an ACP card
 *  whose agent offered no allow-once option, or one Eitri held back because the card cannot show the whole
 *  call. Nothing is sent; sending would only bring back the agent's refusal under a misleading reason. */
export const NOT_APPROVABLE_FLASH = "this card cannot be approved — deny it, or Ctrl+c interrupts the turn";

/** What `<leader>k` says when the agent left nothing running (Rust's `NOTHING_IN_THE_BACKGROUND`). */
export const NOTHING_IN_THE_BACKGROUND = "nothing is running in the background";

/** What `<leader>k` says on a Claude tab: its session does not report what it left running, so "nothing" would
 *  claim more than is known. */
export const CLAUDE_REPORTS_NO_BACKGROUND = "this Claude Code sidecar does not report background work";

/** `<leader>k`'s line when there is nothing to stop, for the tab's agent. */
export function nothingToStopWords(agent: string | undefined): string {
  return agent === "claude" ? CLAUDE_REPORTS_NO_BACKGROUND : NOTHING_IN_THE_BACKGROUND;
}

/** `<leader>k`'s question: what is stopped, by whom it was left running, and what stopping it costs. */
export function stopBackgroundQuestion(who: string, running: number): string {
  const what = running === 1 ? "the command" : `the ${running} commands`;
  return `Stop ${what} ${who} left running? This ends the session; the conversation can be resumed. y/n`;
}

