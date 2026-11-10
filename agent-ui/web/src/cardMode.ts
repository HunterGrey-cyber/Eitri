import {
  CTRL_G_IN_CARD_FLASH,
  REPLAYED_KEY_FLASH,
  SHIFT_TAB_IN_CARD_FLASH,
  cardArmingFlash,
  heldAnswerFlash,
  heldCardChordFlash,
  lettersNeverAnswerFlash,
  modifiedReasonEnterFlash,
  noButtonPickedFlash,
  noSuchAnswerFlash,
  notApprovableFlash,
  shiftedDigitFlash,
} from "./app/flashes";
import { isPlainAnswerKey, type KeyLike } from "./keymap";
import { isShiftTab } from "./modeKey";
import { isModifierKey, TYPING_GUARD_MS } from "./typingGuard";

/** Card mode: the one state in which a key may answer a permission card. A card takes it only when a person
 *  enters it (never by itself, never on an arrival), and while it holds a card every key goes through `cardKey`
 *  first. This module decides; the page applies what it returns and owns the timers, the focus and the flashes.
 *
 *  A key answers only when card mode is armed: `CARD_ARM_MS` after the latest of the entering key's press, the
 *  paint that drew card mode, an arrival on the card's tab, and every key pressed in card mode that did not answer.
 *  A card mode that was never painted is never armed, so a key queued in a stall before anything was drawn cannot
 *  answer, however late it is handled. Every time is a press time on the keydowns' clock (`TypingGuard.timeOf`, or
 *  `clockNow` for a paint and an arrival), never a handling time.
 *
 *  An armed digit or Enter does not answer at once either: it starts the after-half (`answering`), and an answer
 *  stands only if no other key comes in `TYPING_GUARD_MS` after it. Any key in that half cancels it and re-arms from
 *  its own press, so "1. do x" typed into card mode answers nothing. A key `cardKey` never sees as one (Super, Hyper
 *  and the other keys only the page's typing guard counts) is to be passed to `keyCancelsAnswer`. A repeat never
 *  answers; nor does a key the page replayed on its root after a focus loss. No count exists here: a digit is a
 *  button, never the start of a number.
 *
 *  Each card brings its own list of answers (`CardOffer.answers`, digit = place in the list + 1, at most nine). One
 *  rule holds for every card: answer 2 is its deny-like answer, so the same digit refuses everywhere and the lasting
 *  grant is never the middle key. Today's cards: 1 Approve, 2 Deny, 3 Always allow (only where the card offers a saved
 *  rule); a broker or ACP card: 1 Approve, 2 Deny. `enterCardMode` refuses a list that breaks the rule.
 *
 *  A proof is what an answer must carry: `keyAnswerProof` for a key, `pointerAnswerProof` for a click. They are its
 *  only constructors; the types are branded with symbols only they declare (this module, and `./answerProof` for a
 *  pointer's), so nothing else can make one without a cast. A key's proof names the answer itself (the button, and the reason for a deny from the reason
 *  field), so whoever sends the answer reads what to send from the proof and from nothing else. */

/** How long card mode stays unarmed after each of its arming sources. */
export const CARD_ARM_MS = 300;

export type CardRef = { permissionId: string; tab: number };
/** One answer a card offers. `approve` and `always` allow (`always` with a saved rule, `remember`); `deny` is the
 *  deny-like answer, which every card has in place 2. */
export type CardAnswer = { kind: "approve" | "deny" | "always" };
/** What the card offers: its answers in button order (2 to 9 of them, the second deny-like), whether an allowing
 *  answer may be used at all, whether a deny can carry a reason (with the text to show when it cannot), and the
 *  saved rule its `always` answer keeps, as the card draws it (absent or `null` where it offers none). */
export type CardOffer = {
  answers: readonly CardAnswer[];
  approvable: boolean;
  takesReason: boolean;
  noReasonFlash: string;
  rule?: string | null;
};
/** Today's answer lists: a provider card with a saved-rule offer, and every other card. */
export const ANSWERS_WITH_RULE: readonly CardAnswer[] = Object.freeze([
  { kind: "approve" },
  { kind: "deny" },
  { kind: "always" },
] as const);
export const ANSWERS_PLAIN: readonly CardAnswer[] = Object.freeze([{ kind: "approve" }, { kind: "deny" }] as const);

/** Whether `answer` is deny-like: an answer that grants nothing. */
export function isDenyLike(answer: CardAnswer): boolean {
  return answer.kind === "deny";
}

/** Whether a card's answer list is one card mode takes: 2 to 9 answers, the second deny-like. */
export function answersValid(answers: readonly CardAnswer[]): boolean {
  return answers.length >= 2 && answers.length <= 9 && isDenyLike(answers[1]);
}
/** Where card mode was entered from, so the page can go back there: the mode, the cursor row's key, and the
 *  composer's caret when entered from the box. */
export type CardOrigin = { mode: "browse" | "input" | "dock"; cursorKey: string | null; caret: number | null };
export type CardModeState = {
  card: CardRef;
  origin: CardOrigin;
  offer: CardOffer;
  armFrom: number; // keydown clock: the latest of the entry press, the paint, an arrival, a key that did not answer
  painted: boolean; // a paint of card mode has been seen since entry
  highlight: number | null; // 0-based button; null on entry, always
  reasonOpen: boolean;
  answering: { button: number; pressedAt: number; withReason: boolean } | null; // the after-half in flight
};
export type CardKey = KeyLike & { repeat: boolean; replayed: boolean; pressedAt: number; code?: string };
export type CardIntent =
  | {
      kind: "answer";
      button: number;
      answer: CardAnswer;
      decision: "allow" | "deny";
      remember: boolean;
      withReason: boolean;
    }
  | { kind: "type" } // a key typed into the open reason field: neither prevented nor stopped
  | { kind: "moved" } // highlight changed (h l 0 ^ $ Left Right)
  | { kind: "reason"; open: boolean }
  | { kind: "leave" } // Esc (Ctrl+[ arrives as Escape), Ctrl+c
  | { kind: "other-card"; delta: 1 | -1 } // Alt+n / Alt+p
  | { kind: "scroll"; by: "row" | "half"; delta: 1 | -1 } // j k Down Up / Ctrl+d Ctrl+u
  | { kind: "help" } // F1
  | { kind: "ignore" } // a bare modifier
  | { kind: "refuse"; flash: string };

/** Card mode as it starts: armed from the entering key's press once it has been painted, nothing highlighted,
 *  the reason field closed, no answer in flight. */
export function enterCardMode(card: CardRef, origin: CardOrigin, offer: CardOffer, pressedAt: number): CardModeState {
  if (!answersValid(offer.answers)) {
    throw new Error("card mode takes 2 to 9 answers, the second deny-like");
  }
  return {
    card,
    origin,
    offer,
    armFrom: pressedAt,
    painted: false,
    highlight: null,
    reasonOpen: false,
    answering: null,
  };
}

/** The paint that drew card mode (its entry, or a change of its card), at `at` on the keydowns' clock. Only that
 *  paint is reported: a later one moves `armFrom` past an answer in flight, which then answers nothing. */
export function notePaint(s: CardModeState, at: number): CardModeState {
  return { ...s, painted: true, armFrom: Math.max(s.armFrom, at) };
}

/** An arrival on the card's tab at `at`: card mode stays and arms again from it. */
export function noteArrival(s: CardModeState, at: number): CardModeState {
  return { ...s, armFrom: Math.max(s.armFrom, at) };
}

export function isArmed(s: CardModeState, t: number): boolean {
  return s.painted && t >= s.armFrom + CARD_ARM_MS;
}

/** A key pressed at `at` that is not an answer: any answer in flight is dropped, and card mode arms again from the
 *  latest of that answer's press (it did not answer after all) and `at` (this key did not answer either). It re-arms
 *  with nothing in flight too. `cardKey` applies it to every key it reads; the page is to call it for every key its
 *  typing guard counts that `cardKey` ignores as a bare modifier (Super, Hyper and the like). */
export function keyCancelsAnswer(s: CardModeState, at: number): CardModeState {
  return { ...s, answering: null, armFrom: Math.max(s.armFrom, s.answering?.pressedAt ?? -Infinity, at) };
}

/** One keydown while card mode holds a card: the next state and what the page should do with the key. */
export function cardKey(s: CardModeState, ev: CardKey): { state: CardModeState; intent: CardIntent } {
  // A bare modifier is not a key yet. (Super and Hyper are keys to the page's typing guard; for those the page
  // calls `keyCancelsAnswer`.)
  if (isModifierKey(ev.key)) return { state: s, intent: { kind: "ignore" } };

  // Every other key ends an answer still in its after-half before it is read for itself, and re-arms from its own
  // press first, so a key that cancels an answer never answers itself.
  const base = s.answering === null ? s : keyCancelsAnswer(s, ev.pressedAt);
  // Every key that does not answer moves the arming to its own press and drops any answer in flight; every key but
  // the highlight keys clears the highlight.
  const rearmed = (keepHighlight = false): CardModeState => ({
    ...base,
    armFrom: Math.max(base.armFrom, ev.pressedAt),
    answering: null,
    highlight: keepHighlight ? base.highlight : null,
  });
  const not = (intent: CardIntent, state: CardModeState = rearmed()) => ({ state, intent });
  const refuse = (flash: string, state: CardModeState = rearmed()) => not({ kind: "refuse", flash }, state);

  if (ev.replayed) return refuse(REPLAYED_KEY_FLASH);

  const composing = ev.isComposing || ev.keyCode === 229;
  const answers = base.offer.answers;
  const n = answers.length;

  if (base.reasonOpen) {
    // The open reason field takes every key as text except these few. An input method keeps every key it is
    // composing with, its Escape and Enter included.
    if (composing) return not({ kind: "type" });
    if ((isAltOnly(ev, "n") || isAltOnly(ev, "p")) && ev.repeat) return refuse(heldCardChordFlash(chordName(ev)));
    if (isAltOnly(ev, "n")) return not({ kind: "other-card", delta: 1 });
    if (isAltOnly(ev, "p")) return not({ kind: "other-card", delta: -1 });
    if (ev.key === "F1" && !ev.ctrlKey && !ev.altKey && !ev.metaKey) return not({ kind: "help" });
    if (isShiftTab(ev)) return refuse(SHIFT_TAB_IN_CARD_FLASH);
    if ((ev.key === "Tab" && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) || ev.key === "Escape") {
      return not({ kind: "reason", open: false }, { ...rearmed(), reasonOpen: false });
    }
    if (ev.key === "Enter" && isPlainAnswerKey(ev) && !ev.shiftKey) {
      if (ev.repeat) return refuse(heldAnswerFlash("Enter"));
      if (!isArmed(base, ev.pressedAt)) return refuse(cardArmingFlash("Enter"));
      return answer(base, ev, 1, true);
    }
    // Shift, Ctrl, Alt or Meta with Enter is no answer and no text (the field holds one line): refused, so only the
    // plain Enter above denies.
    if (ev.key === "Enter") return refuse(modifiedReasonEnterFlash(keyName(ev)));
    return not({ kind: "type" });
  }

  if (composing) return refuse("");

  const plain = isPlainAnswerKey(ev);
  const unshifted = plain && !ev.shiftKey;

  if (plain && ev.shiftKey && /^[1-9]$/.test(ev.key)) return refuse(shiftedDigitFlash(ev.key));

  if (unshifted && /^[1-9]$/.test(ev.key)) {
    const digit = Number(ev.key);
    if (digit > n) return refuse(noSuchAnswerFlash(ev.key, n));
    if (ev.repeat) return refuse(heldAnswerFlash(ev.key));
    if (!isArmed(base, ev.pressedAt)) return refuse(cardArmingFlash(ev.key));
    if (!isDenyLike(answers[digit - 1]) && !base.offer.approvable) return refuse(notApprovableFlash(ev.key));
    return answer(base, ev, digit - 1, false);
  }

  if (unshifted && (ev.key === "h" || ev.key === "ArrowLeft")) {
    const at = base.highlight === null ? 0 : Math.max(0, base.highlight - 1);
    return not({ kind: "moved" }, { ...rearmed(true), highlight: at });
  }
  if (unshifted && (ev.key === "l" || ev.key === "ArrowRight")) {
    const at = base.highlight === null ? 0 : Math.min(n - 1, base.highlight + 1);
    return not({ kind: "moved" }, { ...rearmed(true), highlight: at });
  }
  // `^` and `$` are shifted on most layouts, so Shift does not matter for them.
  if ((unshifted && ev.key === "0") || (plain && ev.key === "^")) {
    return not({ kind: "moved" }, { ...rearmed(true), highlight: 0 });
  }
  if (plain && ev.key === "$") return not({ kind: "moved" }, { ...rearmed(true), highlight: n - 1 });

  if (unshifted && ev.key === "Enter") {
    if (base.highlight === null) return refuse(noButtonPickedFlash(n));
    if (ev.repeat) return refuse(heldAnswerFlash("Enter"));
    if (!isArmed(base, ev.pressedAt)) return refuse(cardArmingFlash("Enter"));
    if (!isDenyLike(answers[base.highlight]) && !base.offer.approvable) return refuse(notApprovableFlash("Enter"));
    return answer(base, ev, base.highlight, false);
  }

  if (isShiftTab(ev)) return refuse(SHIFT_TAB_IN_CARD_FLASH);
  if (ev.key === "Tab" && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
    if (!base.offer.takesReason) return refuse(base.offer.noReasonFlash);
    return not({ kind: "reason", open: true }, { ...rearmed(), reasonOpen: true });
  }

  // Escape with any modifier: Ctrl+[ arrives as Escape, and leaving answers nothing.
  if (ev.key === "Escape" || isCtrlOnly(ev, "c")) return not({ kind: "leave" });
  if ((isAltOnly(ev, "n") || isAltOnly(ev, "p")) && ev.repeat) return refuse(heldCardChordFlash(chordName(ev)));
  if (isAltOnly(ev, "n")) return not({ kind: "other-card", delta: 1 });
  if (isAltOnly(ev, "p")) return not({ kind: "other-card", delta: -1 });
  if (unshifted && (ev.key === "j" || ev.key === "ArrowDown")) return not({ kind: "scroll", by: "row", delta: 1 });
  if (unshifted && (ev.key === "k" || ev.key === "ArrowUp")) return not({ kind: "scroll", by: "row", delta: -1 });
  if (isCtrlOnly(ev, "d")) return not({ kind: "scroll", by: "half", delta: 1 });
  if (isCtrlOnly(ev, "u")) return not({ kind: "scroll", by: "half", delta: -1 });
  if (ev.key === "F1" && !ev.ctrlKey && !ev.altKey && !ev.metaKey) return not({ kind: "help" });
  if (isCtrlOnly(ev, "g")) return refuse(CTRL_G_IN_CARD_FLASH);
  return refuse(lettersNeverAnswerFlash(keyName(ev), n));
}

/** The proof that a key answered `card`: only for the answer card mode has in flight from that very press, still
 *  armed at that press, on a card still waiting, once its after-half is over (`now`, on the keydowns' clock, at least
 *  `TYPING_GUARD_MS` after the press). Anything that re-armed card mode since (an arrival, another key) or moved it to
 *  another card makes it `null`. The proof names the answer in flight: its button, the decision and `remember` that
 *  button means, and for a deny-like answer the reason (`reason`, the reason field's text, whether the deny came from
 *  the field's Enter or from its digit with the field shut and its text kept, as the Deny button's click sends it;
 *  `null` for an answer that grants). */
export function keyAnswerProof(
  s: CardModeState | null,
  card: CardRef,
  pressedAt: number,
  stillWaiting: boolean,
  now: number,
  reason?: string,
): KeyAnswerProof | null {
  if (s === null || !stillWaiting) return null;
  if (s.card.permissionId !== card.permissionId || s.card.tab !== card.tab) return null;
  if (s.answering === null || s.answering.pressedAt !== pressedAt) return null;
  if (!isArmed(s, pressedAt)) return null;
  if (!(now >= pressedAt + TYPING_GUARD_MS)) return null;
  const { button, withReason } = s.answering;
  const chosen = s.offer.answers[button];
  // The brand is a type only: no value carries it at run time.
  return Object.freeze({
    card: Object.freeze({ ...card }),
    pressedAt,
    button,
    answer: Object.freeze({ ...chosen }),
    decision: isDenyLike(chosen) ? "deny" : "allow",
    remember: chosen.kind === "always",
    rule: chosen.kind === "always" ? (s.offer.rule ?? null) : null,
    withReason,
    reason: isDenyLike(chosen) ? (reason ?? "") : null,
  }) as KeyAnswerProof;
}

/** Whether a key's proof answers exactly this call: the card and tab it names, the decision and `remember` its
 *  button means, and for a lasting grant the very rule the card showed when the key was pressed (`offeredRule` is
 *  the rule offered now). A call that disagrees with its proof in any of these sends nothing. */
export function proofAnswers(
  proof: KeyAnswerProof,
  permissionId: string,
  tab: number,
  decision: "allow" | "deny",
  remember: boolean,
  offeredRule: string | null,
): boolean {
  if (proof.card.permissionId !== permissionId || proof.card.tab !== tab) return false;
  if (proof.decision !== decision || proof.remember !== remember) return false;
  return !remember || (proof.rule !== null && proof.rule === offeredRule);
}

// A pointer's proof: a click a real press made (`detail` > 0). Its own module, which imports nothing, so a card's
// markup can make one without this module's imports.
export { pointerAnswerProof, type PointerAnswerProof } from "./answerProof";

// What an answer must carry; `keyAnswerProof` and `pointerAnswerProof` are the only constructors.
declare const keyBrand: unique symbol;
export type KeyAnswerProof = {
  readonly [keyBrand]: true;
  readonly card: CardRef;
  readonly pressedAt: number;
  /** The chosen answer's place in the card's list (0-based; digit - 1), and the answer itself. */
  readonly button: number;
  readonly answer: CardAnswer;
  readonly decision: "allow" | "deny";
  readonly remember: boolean;
  /** The rule a lasting grant (`remember`) keeps, as the card showed it; `null` for every other answer. */
  readonly rule: string | null;
  readonly withReason: boolean;
  readonly reason: string | null;
};

function answer(
  s: CardModeState,
  ev: CardKey,
  button: number,
  withReason: boolean,
): { state: CardModeState; intent: CardIntent } {
  const chosen = s.offer.answers[button];
  return {
    state: { ...s, highlight: null, answering: { button, pressedAt: ev.pressedAt, withReason } },
    intent: {
      kind: "answer",
      button,
      answer: chosen,
      decision: isDenyLike(chosen) ? "deny" : "allow",
      remember: chosen.kind === "always",
      withReason,
    },
  };
}

function hasAltGraph(ev: KeyLike): boolean {
  return ev.getModifierState?.("AltGraph") === true;
}

/** `Alt` with `letter` and nothing else held (Caps Lock does not matter). */
function isAltOnly(ev: KeyLike, letter: string): boolean {
  return (
    ev.altKey === true &&
    !ev.ctrlKey &&
    !ev.metaKey &&
    !ev.shiftKey &&
    !hasAltGraph(ev) &&
    ev.key.toLowerCase() === letter
  );
}

/** `Ctrl` with `letter` and nothing else held (Caps Lock does not matter). */
function isCtrlOnly(ev: KeyLike, letter: string): boolean {
  return ev.ctrlKey && !ev.altKey && !ev.metaKey && !ev.shiftKey && ev.key.toLowerCase() === letter;
}

/** A key as the band names it: its modifiers, then the key, `Space` for the space bar. Shift is named only on a
 *  key whose name it does not already change. */
/** `Alt-n` / `Alt-p`, as the band and the keys sheet write the chords. */
function chordName(ev: KeyLike): string {
  return `Alt-${ev.key.toLowerCase()}`;
}

function keyName(ev: KeyLike): string {
  const name = ev.key === " " ? "Space" : ev.key;
  const mods: string[] = [];
  if (ev.ctrlKey) mods.push("Ctrl");
  if (ev.altKey) mods.push("Alt");
  if (ev.metaKey) mods.push("Meta");
  if (ev.shiftKey && name.length > 1) mods.push("Shift");
  return [...mods, name].join("+");
}
