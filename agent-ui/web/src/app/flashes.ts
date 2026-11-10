/** The short form the band shows of a flash too wide for its one row (`bandLayout`), tried once every segment that
 *  can go has gone and the whole sentence does not fit beside the mode's letter either: a narrow chat (a column of a
 *  split window, a laptop at zoom 1.5) still says what to do, where a cut (`cutFlash`) would keep only the sentence's
 *  start. The whole sentence stays the segment's title. A flash without a short form is cut, and so is a short form
 *  that does not fit either. Each form is written beside its sentence (`withShort`), and a sentence built from a key
 *  or a count records its form when it is built, so every flash the band can be handed already has its entry. */
const SHORT = new Map<string, string>();
export const SHORT_FLASH: ReadonlyMap<string, string> = SHORT;
/** `full`, with `short` recorded as its short form. */
function withShort(full: string, short: string): string {
  SHORT.set(full, short);
  return full;
}

/** What the band says when `Esc` is pressed while a turn runs: it never interrupts, and this is the reminder. */
export const ESC_DOES_NOT_INTERRUPT_FLASH = withShort("Esc does not interrupt — ctrl+c does", "ctrl+c interrupts");

/** BROWSE `a`, `d` or `D` on a card's row, or on a tool row a card gates: cards are answered in card mode only. */
export function cardKeyGoneFlash(key: string): string {
  return withShort(`${key}: cards are answered in card mode — Enter or Alt-n, then 1-3`, `${key}: Alt-n, then 1-3`);
}
/** BROWSE `a`, `d` or `D` anywhere else: not a key of this table. */
export function notAKeyHereFlash(key: string): string {
  return withShort(`${key}: not a key here — F1 lists the keys`, `${key}: not a key — F1`);
}
/** `]p` / `[p`, which once walked the waiting cards: card mode's `Alt-n` / `Alt-p` do. */
export function cardJumpGoneFlash(key: string): string {
  return withShort(`${key}: cards are on Alt-n / Alt-p now`, `${key}: use ${key.startsWith("]") ? "Alt-n" : "Alt-p"}`);
}
/** BROWSE `Ctrl+o` / `Ctrl+i` with nothing older / newer in this tab's jumplist. */
export const NO_OLDER_JUMP_FLASH = "no older jump";
export const NO_NEWER_JUMP_FLASH = "no newer jump";
/** What the band says when Enter or Space on the activity line's
 *  Stop did not interrupt because it came in the middle of typing (`TypingGuard.mayActAfterMotion`)
 *  -- "just do it" typed after an arrival reached Stop with its `j` and interrupted the turn. */
export const STOP_TYPING_FLASH = "Stop takes a key only on its own — i or Ctrl+j to type";
/** What the band says on INPUT's `Ctrl+y`, which answers no card: cards are answered in card mode, entered with
 *  `Alt-n`. */
export const CTRL_Y_GONE_FLASH = withShort("Ctrl+y: cards are on Alt-n now", "Ctrl+y: use Alt-n");
/** Card mode (`../cardMode`). What the band says when an answer in its after-half was cancelled by the key that
 *  followed it within `TYPING_GUARD_MS`. */
export const ANSWER_CANCELLED_FLASH = withShort("answer cancelled: another key followed", "cancelled: key followed");
/** An answer in its after-half cancelled by something that was not a key: the card's tab arriving again, or the
 *  panel taking the keys or the view (`answerEpoch`). */
export const ANSWER_INTERRUPTED_FLASH = withShort("answer cancelled before it was sent — press it again", "cancelled — press again");
/** Card mode ended because what its card offers changed (a rule offer came, went or was replaced). */
export const CARD_CHANGED_FLASH = withShort("card changed — Alt-n or Enter again", "card changed — Alt-n");
/** `Alt-n`/`Alt-p` found no card waiting in this tab. */
export const NO_CARD_WAITING_FLASH = "no card waiting";
/** `Alt-n`/`Alt-p` found no card, but a question waits: questions are not in card mode's ring. */
export const QUESTION_WAITS_FLASH = withShort("a question waits — j to it, Enter", "a question waits — j");
/** BROWSE Enter on a card row (or a tool row a card gates) that did not stand alone: card mode needs a lone press. */
export const ENTER_ALONE_FLASH = withShort("Enter: press it on its own to reach the card", "Enter: press it alone");
/** Enter in a card's reason box outside card mode: the box answers nothing until card mode opens it. */
export const REASON_ENTER_OUTSIDE_FLASH = withShort(
  "Enter: card mode first — Enter or Alt-n on the card, then Tab",
  "Enter: Alt-n first",
);
/** Card mode ended because its card was answered somewhere else (a click, the phone), or is gone (withdrawn by the
 *  agent, the session ended). */
export const CARD_ANSWERED_ELSEWHERE_FLASH = "card answered elsewhere";
export const CARD_WITHDRAWN_FLASH = "card withdrawn";
/** What entering card mode dropped: a pending prefix (`g`, `z`, `[`, `]`, `Ctrl+w`, a leader sequence) or a count. */
export function pendingDroppedFlash(pending: string): string {
  return `pending ${pending} dropped`;
}
export const COUNT_DROPPED_FLASH = "count dropped";
/** After card mode answered a card or was left, when more cards wait in the tab: it never moves on to them. */
export function moreWaitingFlash(n: number): string {
  return `${n} more waiting — Alt-n`;
}
/** Why a card's deny takes no reason (Tab in card mode): a broker card, and another agent's card. */
export const BROKER_NO_REASON_FLASH = withShort("an editor card takes no reason with a deny; 2 denies", "no reason — 2 denies");
export function agentNoReasonFlash(agent: string): string {
  return withShort(`${agent} takes no reason with a deny; 2 denies`, "no reason — 2 denies");
}

/** What the band says when card mode's `key` (a digit, or Enter on a highlight) would allow on a card whose Approve
 *  is disabled: an ACP card whose agent offered no allow-once option, or one Eitri held back because the card cannot
 *  show the whole call. Nothing is sent; sending would only bring back the agent's refusal under a misleading reason.
 *  In card mode `Ctrl+c` leaves and interrupts nothing, so the way out named is Esc. */
export function notApprovableFlash(key: string): string {
  return withShort(`${key}: this card cannot be approved — 2 denies, Esc leaves`, `${key}: cannot approve`);
}

/* Card mode's refusals (`../cardMode`). Each names the key first, as the band reads it. */
export const REPLAYED_KEY_FLASH = withShort("key replayed after a focus loss — press again", "replayed — press again");
export const SHIFT_TAB_IN_CARD_FLASH = withShort("Shift+Tab: answer or leave the card first (Esc)", "Shift+Tab: Esc first");
export const CTRL_G_IN_CARD_FLASH = withShort("Ctrl+g: leave card mode first — Esc", "Ctrl+g: Esc first");
/** A digit typed with Shift held (on AZERTY every digit is): never an answer, since the shifted key may be another
 *  character on another layout. */
export function shiftedDigitFlash(digit: string): string {
  return withShort(`Shift+${digit}: digits answer without Shift — h/l then Enter works too`, `Shift+${digit}: drop Shift`);
}
export function heldAnswerFlash(key: string): string {
  return withShort(`${key}: a held key answers nothing — press it again`, `${key} held: press again`);
}
/** A digit or Enter pressed before card mode is armed (inside `CARD_ARM_MS` of its entry, its paint, an arrival or a
 *  key that did not answer). */
export function cardArmingFlash(key: string): string {
  return withShort(`${key}: card arming — press again`, `${key}: not armed yet`);
}
export function noSuchAnswerFlash(digit: string, answers: number): string {
  return withShort(`${digit}: this card has ${answers} answers`, `${digit}: only 1-${answers}`);
}
export function noButtonPickedFlash(answers: number): string {
  return withShort(`Enter: no button picked — 1-${answers}, or h/l then Enter`, "Enter: pick with h/l");
}
/** Enter with a modifier in the open reason field: only a plain Enter denies with the reason. */
export function modifiedReasonEnterFlash(key: string): string {
  return withShort(`${key}: Enter alone denies with the reason`, `${key}: just Enter`);
}
/** A held `Alt+n`/`Alt+p`: its repeats move card mode to no other card. */
export function heldCardChordFlash(key: string): string {
  return withShort(`${key}: a held key moves to no card — press it again`, `${key} held: press again`);
}
/** Any other key in card mode. A letter is told letters never answer; any other key (`?`, `Ctrl+x`, `F5`) that it is
 *  none of card mode's keys. Both name the card's own digits. */
export function lettersNeverAnswerFlash(key: string, answers: number): string {
  const why = /^[A-Za-z]$/.test(key) ? "letters never answer a card" : "not a card-mode key";
  return withShort(`${key}: ${why} — 1-${answers}, or h/l then Enter; Esc leaves`, `${key}: 1-${answers} or Esc`);
}

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

