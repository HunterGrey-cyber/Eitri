import { describe, expect, it } from "vitest";
import { notApprovableFlash } from "./app/flashes";
import { TYPING_GUARD_MS } from "./typingGuard";
import {
  ANSWERS_PLAIN,
  ANSWERS_WITH_RULE,
  CARD_ARM_MS,
  answersValid,
  cardKey,
  denyDigit,
  enterCardMode,
  isArmed,
  keyAnswerProof,
  keyCancelsAnswer,
  notePaint,
  noteArrival,
  pointerAnswerProof,
  proofAnswers,
  type CardAnswer,
  type CardKey,
  type CardModeState,
  type CardOffer,
  type CardOrigin,
  type CardRef,
} from "./cardMode";

/* The card-mode machine, pure: every time below is a press time on the keydowns' clock, chosen by the test. A card
   mode entered at `T0` and painted at `T0` is armed from `ARMED`. */

const T0 = 1000;
const ARMED = T0 + CARD_ARM_MS;
/** A moment long after every press below: any after-half is over by then. */
const LATE = T0 + 1e6;

const CARD: CardRef = { permissionId: "perm-1", tab: 1 };
const BROWSE: CardOrigin = { mode: "browse", cursorKey: "row-7", caret: null };
const INPUT: CardOrigin = { mode: "input", cursorKey: null, caret: 4 };
const DOCK: CardOrigin = { mode: "dock", cursorKey: null, caret: null };
/** A provider card with a saved-rule offer: three answers and a reason. */
const PROVIDER: CardOffer = { answers: ANSWERS_WITH_RULE, approvable: true, takesReason: true, noReasonFlash: "unused" };
/** A provider card without a rule offer. */
const PROVIDER_2: CardOffer = { answers: ANSWERS_PLAIN, approvable: true, takesReason: true, noReasonFlash: "unused" };
/** An editor (broker) card: two answers, no reason. */
const BROKER: CardOffer = {
  answers: ANSWERS_PLAIN,
  approvable: true,
  takesReason: false,
  noReasonFlash: "an editor card takes no reason with a deny; 2 denies",
};
/** An ACP card the agent offered no `allow_once` for. */
const ACP_NO_ALLOW: CardOffer = { answers: ANSWERS_PLAIN, approvable: false, takesReason: false, noReasonFlash: "no reason here" };

function key(k: string, at: number, extra: Partial<CardKey> = {}): CardKey {
  return {
    key: k,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    isComposing: false,
    keyCode: 0,
    repeat: false,
    replayed: false,
    pressedAt: at,
    ...extra,
  };
}

function entered(offer: CardOffer = PROVIDER, origin: CardOrigin = BROWSE): CardModeState {
  return notePaint(enterCardMode(CARD, origin, offer, T0), T0);
}

/** Feed keys in order; the last result. */
function press(s: CardModeState, ...keys: CardKey[]) {
  let state = s;
  let last = { state, intent: { kind: "ignore" } as ReturnType<typeof cardKey>["intent"] };
  for (const ev of keys) {
    last = cardKey(state, ev);
    state = last.state;
  }
  return last;
}

function refused(flash: string) {
  return { kind: "refuse", flash };
}

describe("cardKey: bare modifiers", () => {
  it("a bare modifier is ignored and changes nothing, an answer in flight included", () => {
    for (const s of [entered(), cardKey(entered(), key("1", ARMED)).state]) {
      for (const m of ["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]) {
        const r = cardKey(s, key(m, ARMED + 100));
        expect(r.intent).toEqual({ kind: "ignore" });
        expect(r.state).toBe(s);
      }
    }
  });
});

describe("cardKey: every key that does not answer re-arms and drops an answer in flight", () => {
  it("re-arms from its own press and clears answering", () => {
    const answering = cardKey(entered(), key("3", ARMED)).state;
    expect(answering.answering).toEqual({ button: 2, pressedAt: ARMED, withReason: false });
    for (const k of [key("j", ARMED + 100), key("x", ARMED + 100), key("Escape", ARMED + 100), key("Tab", ARMED + 100)]) {
      const r = cardKey(answering, k);
      expect(r.intent.kind).not.toBe("answer");
      expect(r.state.answering).toBeNull();
      expect(r.state.armFrom).toBe(ARMED + 100);
    }
  });

  it("never moves the arming back", () => {
    const s = noteArrival(entered(), T0 + 500);
    expect(cardKey(s, key("x", T0 + 200)).state.armFrom).toBe(T0 + 500);
  });

  it("every key but h l 0 ^ $ Left Right clears the highlight", () => {
    const lit = cardKey(entered(), key("l", ARMED)).state;
    expect(lit.highlight).toBe(0);
    for (const k of ["h", "l", "0", "^", "$", "ArrowLeft", "ArrowRight"]) {
      expect(cardKey(lit, key(k, ARMED + 10, k === "^" || k === "$" ? { shiftKey: true } : {})).state.highlight).not.toBeNull();
    }
    for (const k of ["j", "k", "x", "Tab", "Escape", "4", "F1", "ArrowUp", "ArrowDown"]) {
      expect(cardKey(lit, key(k, ARMED + 10)).state.highlight).toBeNull();
    }
    expect(cardKey(lit, key("d", ARMED + 10, { ctrlKey: true })).state.highlight).toBeNull();
    expect(cardKey(lit, key("n", ARMED + 10, { altKey: true })).state.highlight).toBeNull();
  });
});

describe("cardKey: composing and replayed keys", () => {
  it("a composing key is refused with no flash and re-arms", () => {
    for (const extra of [{ isComposing: true }, { keyCode: 229 }]) {
      for (const k of ["1", "Enter", "a", "Escape"]) {
        const r = cardKey(entered(), key(k, ARMED + 50, extra));
        expect(r.intent).toEqual(refused(""));
        expect(r.state.armFrom).toBe(ARMED + 50);
      }
    }
  });

  it("a replayed key is refused, armed digit or not, and re-arms", () => {
    for (const k of ["1", "2", "Enter", "a"]) {
      const r = cardKey(entered(), key(k, ARMED + 50, { replayed: true }));
      expect(r.intent).toEqual(refused("key replayed after a focus loss — press again"));
      expect(r.state.armFrom).toBe(ARMED + 50);
      expect(r.state.answering).toBeNull();
    }
  });

  it("a replayed key is refused in the open reason field too, before it is typed", () => {
    const open = cardKey(entered(), key("Tab", T0)).state;
    expect(cardKey(open, key("x", T0 + 900, { replayed: true })).intent).toEqual(
      refused("key replayed after a focus loss — press again"),
    );
  });
});

describe("cardKey: digits", () => {
  it("1 2 3 answer Approve, Always, Deny (Claude Code's order) once armed, starting the after-half", () => {
    const want = [
      { button: 0, answer: { kind: "approve" }, decision: "allow", remember: false },
      { button: 1, answer: { kind: "always" }, decision: "allow", remember: true },
      { button: 2, answer: { kind: "deny" }, decision: "deny", remember: false },
    ];
    ["1", "2", "3"].forEach((d, i) => {
      const r = cardKey(entered(), key(d, ARMED));
      expect(r.intent).toEqual({ kind: "answer", ...want[i], withReason: false });
      expect(r.state.answering).toEqual({ button: i, pressedAt: ARMED, withReason: false });
      expect(r.state.armFrom).toBe(T0);
    });
  });

  it("1 2 answer Approve, Deny on a card without a rule offer: 2 is Deny there, 3 is Deny beside Always", () => {
    for (const offer of [PROVIDER_2, BROKER]) {
      expect(cardKey(entered(offer), key("1", ARMED)).intent).toMatchObject({ kind: "answer", button: 0, decision: "allow", remember: false });
      expect(cardKey(entered(offer), key("2", ARMED)).intent).toMatchObject({ kind: "answer", button: 1, answer: { kind: "deny" }, decision: "deny", remember: false });
    }
    expect(cardKey(entered(PROVIDER), key("2", ARMED)).intent).toMatchObject({ decision: "allow", remember: true });
    expect(cardKey(entered(PROVIDER), key("3", ARMED)).intent).toMatchObject({ decision: "deny", remember: false });
  });

  it("a held digit answers nothing", () => {
    const r = cardKey(entered(), key("1", ARMED + 500, { repeat: true }));
    expect(r.intent).toEqual(refused("1: a held key answers nothing — press it again"));
    expect(r.state.answering).toBeNull();
  });

  it("an unarmed digit answers nothing", () => {
    const r = cardKey(entered(), key("2", ARMED - 1));
    expect(r.intent).toEqual(refused("2: card arming — press again"));
    expect(r.state.armFrom).toBe(ARMED - 1);
  });

  it("a digit past the card's answers names how many it has", () => {
    expect(cardKey(entered(PROVIDER_2), key("3", ARMED)).intent).toEqual(refused("3: this card has 2 answers"));
    for (const d of ["4", "5", "6", "7", "8", "9"]) {
      expect(cardKey(entered(PROVIDER), key(d, ARMED)).intent).toEqual(refused(`${d}: this card has 3 answers`));
      expect(cardKey(entered(BROKER), key(d, ARMED)).intent).toEqual(refused(`${d}: this card has 2 answers`));
    }
  });

  it("1 on a card that cannot be approved is refused", () => {
    const r = cardKey(entered(ACP_NO_ALLOW), key("1", ARMED));
    expect(r.intent).toEqual(refused(notApprovableFlash("1", 2)));
    expect(notApprovableFlash("1", 2)).toBe("1: this card cannot be approved — 2 denies, Esc leaves");
    expect(r.state.answering).toBeNull();
    expect(cardKey(entered(ACP_NO_ALLOW), key("2", ARMED)).intent).toMatchObject({ kind: "answer", decision: "deny" });
  });

  it("2 (Always) is an allow too on a rule card: refused on one that cannot be approved, naming 3 as its deny", () => {
    const odd: CardOffer = { ...PROVIDER, approvable: false };
    expect(cardKey(entered(odd), key("2", ARMED)).intent).toEqual(refused(notApprovableFlash("2", 3)));
    expect(notApprovableFlash("2", 3)).toBe("2: this card cannot be approved — 3 denies, Esc leaves");
    expect(cardKey(entered(odd), key("1", ARMED)).intent).toEqual(refused(notApprovableFlash("1", 3)));
    expect(cardKey(entered(odd), key("3", ARMED)).intent).toMatchObject({ kind: "answer", decision: "deny" });
  });

  it("a digit with Shift or any other modifier is not a digit", () => {
    // Shift+digit (every digit on AZERTY) has its own text, which names Shift.
    expect(cardKey(entered(), key("1", ARMED, { shiftKey: true })).intent).toEqual(
      refused("Shift+1: digits answer without Shift — h/l then Enter works too"),
    );
    expect(cardKey(entered(BROKER), key("3", ARMED, { shiftKey: true })).intent).toEqual(
      refused("Shift+3: digits answer without Shift — h/l then Enter works too"),
    );
    expect(cardKey(entered(), key("1", ARMED, { ctrlKey: true })).intent).toEqual(
      refused("Ctrl+1: not a card-mode key — 1-3, or h/l then Enter; Esc leaves"),
    );
    expect(cardKey(entered(), key("2", ARMED, { altKey: true })).intent.kind).toBe("refuse");
    expect(
      cardKey(entered(), key("2", ARMED, { getModifierState: (m: string) => m === "AltGraph" })).intent.kind,
    ).toBe("refuse");
  });
});

describe("cardKey: the highlight", () => {
  it("l and h from nothing highlight the first button", () => {
    expect(press(entered(), key("l", ARMED)).state.highlight).toBe(0);
    expect(press(entered(), key("h", ARMED)).state.highlight).toBe(0);
    expect(press(entered(), key("ArrowRight", ARMED)).state.highlight).toBe(0);
    expect(press(entered(), key("ArrowLeft", ARMED)).state.highlight).toBe(0);
  });

  it("l and h move one button and do not wrap", () => {
    const s = entered();
    expect(press(s, key("l", 1), key("l", 2)).state.highlight).toBe(1);
    expect(press(s, key("l", 1), key("l", 2), key("l", 3), key("l", 4)).state.highlight).toBe(2);
    expect(press(entered(PROVIDER_2), key("l", 1), key("l", 2), key("l", 3)).state.highlight).toBe(1);
    expect(press(s, key("$", 1, { shiftKey: true }), key("ArrowLeft", 2)).state.highlight).toBe(1);
    expect(press(s, key("l", 1), key("h", 2), key("h", 3)).state.highlight).toBe(0);
  });

  it("0 and ^ highlight the first button, $ the last", () => {
    const s = press(entered(), key("l", 1), key("l", 2)).state;
    expect(press(s, key("0", 3)).state.highlight).toBe(0);
    expect(press(s, key("^", 3, { shiftKey: true })).state.highlight).toBe(0);
    expect(press(entered(), key("$", 3, { shiftKey: true })).state.highlight).toBe(2);
    expect(press(entered(BROKER), key("$", 3, { shiftKey: true })).state.highlight).toBe(1);
  });

  it("each highlight key says moved and re-arms from its press", () => {
    for (const [k, extra] of [["h", {}], ["l", {}], ["0", {}], ["^", { shiftKey: true }], ["$", { shiftKey: true }],
      ["ArrowLeft", {}], ["ArrowRight", {}]] as const) {
      const r = cardKey(entered(), key(k, ARMED + 40, extra));
      expect(r.intent).toEqual({ kind: "moved" });
      expect(r.state.armFrom).toBe(ARMED + 40);
    }
  });
});

describe("cardKey: Enter", () => {
  it("Enter with nothing highlighted picks nothing", () => {
    expect(cardKey(entered(), key("Enter", ARMED)).intent).toEqual(
      refused("Enter: no button picked — 1-3, or h/l then Enter"),
    );
    expect(cardKey(entered(BROKER), key("Enter", ARMED)).intent).toEqual(
      refused("Enter: no button picked — 1-2, or h/l then Enter"),
    );
  });

  it("Enter answers the highlighted button once armed after the move", () => {
    const lit = press(entered(), key("l", ARMED), key("l", ARMED + 10)).state;
    const r = cardKey(lit, key("Enter", ARMED + 10 + CARD_ARM_MS));
    expect(r.intent).toEqual({ kind: "answer", button: 1, answer: { kind: "always" }, decision: "allow", remember: true, withReason: false });
    expect(r.state.answering).toEqual({ button: 1, pressedAt: ARMED + 10 + CARD_ARM_MS, withReason: false });
    const deny = press(entered(), key("$", ARMED, { shiftKey: true })).state;
    expect(cardKey(deny, key("Enter", ARMED + CARD_ARM_MS)).intent).toMatchObject({ button: 2, decision: "deny", remember: false });
  });

  it("Enter right after the move is unarmed", () => {
    const lit = press(entered(), key("l", ARMED)).state;
    expect(cardKey(lit, key("Enter", ARMED + 100)).intent).toEqual(refused("Enter: card arming — press again"));
  });

  it("a held Enter answers nothing", () => {
    const lit = press(entered(), key("l", ARMED)).state;
    expect(cardKey(lit, key("Enter", ARMED + 900, { repeat: true })).intent).toEqual(
      refused("Enter: a held key answers nothing — press it again"),
    );
  });

  it("Enter on Approve of a card that cannot be approved is refused", () => {
    const lit = press(entered(ACP_NO_ALLOW), key("l", ARMED)).state;
    expect(cardKey(lit, key("Enter", ARMED + CARD_ARM_MS)).intent).toEqual(refused(notApprovableFlash("Enter", 2)));
  });

  it("Enter with Shift or Ctrl answers nothing", () => {
    const lit = press(entered(), key("l", ARMED)).state;
    expect(cardKey(lit, key("Enter", ARMED + 900, { shiftKey: true })).intent.kind).toBe("refuse");
    expect(cardKey(lit, key("Enter", ARMED + 900, { ctrlKey: true })).intent.kind).toBe("refuse");
  });
});

describe("cardKey: Tab and the reason field", () => {
  it("Tab opens the reason field on a card that takes a reason, and closes it", () => {
    const r = cardKey(entered(), key("Tab", ARMED));
    expect(r.intent).toEqual({ kind: "reason", open: true });
    expect(r.state.reasonOpen).toBe(true);
    const back = cardKey(r.state, key("Tab", ARMED + 50));
    expect(back.intent).toEqual({ kind: "reason", open: false });
    expect(back.state.reasonOpen).toBe(false);
    expect(back.state.armFrom).toBe(ARMED + 50);
  });

  it("Tab on a card with no reason is refused with the card's own text", () => {
    const r = cardKey(entered(BROKER), key("Tab", ARMED));
    expect(r.intent).toEqual(refused(BROKER.noReasonFlash));
    expect(r.state.reasonOpen).toBe(false);
  });
});

describe("cardKey: the other keys of card mode", () => {
  it("Esc, Ctrl+[ (as Escape) and Ctrl+c leave", () => {
    expect(cardKey(entered(), key("Escape", ARMED)).intent).toEqual({ kind: "leave" });
    expect(cardKey(entered(), key("Escape", ARMED, { ctrlKey: true })).intent).toEqual({ kind: "leave" });
    expect(cardKey(entered(), key("c", ARMED, { ctrlKey: true })).intent).toEqual({ kind: "leave" });
    expect(cardKey(entered(), key("C", ARMED, { ctrlKey: true })).intent).toEqual({ kind: "leave" });
  });

  it("Alt+n and Alt+p go to the next and previous card, Alt alone", () => {
    expect(cardKey(entered(), key("n", ARMED, { altKey: true })).intent).toEqual({ kind: "other-card", delta: 1 });
    expect(cardKey(entered(), key("p", ARMED, { altKey: true })).intent).toEqual({ kind: "other-card", delta: -1 });
    expect(cardKey(entered(), key("n", ARMED, { altKey: true, ctrlKey: true })).intent.kind).toBe("refuse");
    expect(cardKey(entered(), key("N", ARMED, { altKey: true, shiftKey: true })).intent.kind).toBe("refuse");
    expect(cardKey(entered(), key("n", ARMED)).intent.kind).toBe("refuse");
  });

  it("a held Alt+n or Alt+p moves to no card: its repeat is refused and re-arms", () => {
    for (const k of ["n", "p"]) {
      const r = cardKey(entered(), key(k, ARMED, { altKey: true, repeat: true }));
      expect(r.intent).toEqual(refused(`Alt-${k}: a held key moves to no card — press it again`));
      expect(r.state.armFrom).toBe(ARMED);
    }
  });

  it("j k Down Up scroll by row, Ctrl+d Ctrl+u by half a page", () => {
    expect(cardKey(entered(), key("j", ARMED)).intent).toEqual({ kind: "scroll", by: "row", delta: 1 });
    expect(cardKey(entered(), key("ArrowDown", ARMED)).intent).toEqual({ kind: "scroll", by: "row", delta: 1 });
    expect(cardKey(entered(), key("k", ARMED)).intent).toEqual({ kind: "scroll", by: "row", delta: -1 });
    expect(cardKey(entered(), key("ArrowUp", ARMED)).intent).toEqual({ kind: "scroll", by: "row", delta: -1 });
    expect(cardKey(entered(), key("d", ARMED, { ctrlKey: true })).intent).toEqual({ kind: "scroll", by: "half", delta: 1 });
    expect(cardKey(entered(), key("u", ARMED, { ctrlKey: true })).intent).toEqual({ kind: "scroll", by: "half", delta: -1 });
  });

  it("F1 asks for help", () => {
    expect(cardKey(entered(), key("F1", ARMED)).intent).toEqual({ kind: "help" });
  });

  it("Shift+Tab is refused, as a keymap sends it and as WebKitGTK does", () => {
    const flash = "Shift+Tab: answer or leave the card first (Esc)";
    expect(cardKey(entered(), key("Tab", ARMED, { shiftKey: true })).intent).toEqual(refused(flash));
    expect(cardKey(entered(), key("Unidentified", ARMED, { shiftKey: true, code: "Tab" })).intent).toEqual(
      refused(flash),
    );
    expect(cardKey(entered(), key("Tab", ARMED, { shiftKey: true })).state.reasonOpen).toBe(false);
  });

  it("Ctrl+g is refused: card mode is left first", () => {
    expect(cardKey(entered(), key("g", ARMED, { ctrlKey: true })).intent).toEqual(
      refused("Ctrl+g: leave card mode first — Esc"),
    );
  });

  it("any other key is refused with the card's own digits", () => {
    for (const k of ["a", "d", "D", "y", "n", "g", "i"]) {
      expect(cardKey(entered(), key(k, ARMED)).intent).toEqual(
        refused(`${k}: letters never answer a card — 1-3, or h/l then Enter; Esc leaves`),
      );
    }
    // A key that is no letter is told it is none of card mode's keys.
    for (const k of ["?", ":"]) {
      expect(cardKey(entered(), key(k, ARMED)).intent).toEqual(
        refused(`${k}: not a card-mode key — 1-3, or h/l then Enter; Esc leaves`),
      );
    }
    expect(cardKey(entered(BROKER), key("a", ARMED)).intent).toEqual(
      refused("a: letters never answer a card — 1-2, or h/l then Enter; Esc leaves"),
    );
    expect(cardKey(entered(), key(" ", ARMED)).intent).toEqual(
      refused("Space: not a card-mode key — 1-3, or h/l then Enter; Esc leaves"),
    );
    expect(cardKey(entered(), key("x", ARMED, { ctrlKey: true })).intent).toEqual(
      refused("Ctrl+x: not a card-mode key — 1-3, or h/l then Enter; Esc leaves"),
    );
  });

  it("0 highlights and 4-9 name the card's answers: no digit lacks a meaning", () => {
    expect(cardKey(entered(), key("0", ARMED)).intent).toEqual({ kind: "moved" });
    expect(cardKey(entered(), key("7", ARMED)).intent).toEqual(refused("7: this card has 3 answers"));
  });

  it("counts do not exist: digits never accumulate", () => {
    // `2` unarmed, then `3` once armed again: button 3 answers alone, never "23".
    const r = press(entered(), key("2", ARMED - 100), key("3", ARMED - 100 + CARD_ARM_MS));
    expect(r.intent).toMatchObject({ kind: "answer", button: 2 });
    expect(Object.keys(r.state).sort()).toEqual(
      ["answering", "armFrom", "card", "highlight", "offer", "origin", "painted", "reasonOpen"],
    );
  });
});

describe("cardKey: the open reason field", () => {
  const open = () => cardKey(entered(), key("Tab", T0)).state;

  it("Tab or Escape outside composition closes it, card mode kept and re-armed", () => {
    for (const k of ["Tab", "Escape"]) {
      const r = cardKey(open(), key(k, T0 + 700));
      expect(r.intent).toEqual({ kind: "reason", open: false });
      expect(r.state.reasonOpen).toBe(false);
      expect(r.state.armFrom).toBe(T0 + 700);
    }
  });

  it("Enter outside composition denies with the reason once armed, through the after-half", () => {
    const r = cardKey(open(), key("Enter", T0 + CARD_ARM_MS));
    expect(r.intent).toEqual({ kind: "answer", button: 2, answer: { kind: "deny" }, decision: "deny", remember: false, withReason: true });
    expect(r.state.answering).toEqual({ button: 2, pressedAt: T0 + CARD_ARM_MS, withReason: true });
    // On a card without a rule offer the deny is its second answer, and the field's Enter answers with that one.
    const plain = cardKey(cardKey(entered(PROVIDER_2), key("Tab", T0)).state, key("Enter", T0 + CARD_ARM_MS));
    expect(plain.intent).toEqual({ kind: "answer", button: 1, answer: { kind: "deny" }, decision: "deny", remember: false, withReason: true });
  });

  it("a second Enter in the field's after-half re-arms from its own press and answers nothing", () => {
    const answering = cardKey(open(), key("Enter", ARMED)).state;
    const r = cardKey(answering, key("Enter", ARMED + 100));
    expect(r.intent).toEqual(refused("Enter: card arming — press again"));
    expect(r.state.answering).toBeNull();
    expect(r.state.armFrom).toBe(ARMED + 100);
  });

  it("a shifted digit in the field is typed, as on AZERTY", () => {
    expect(cardKey(open(), key("1", ARMED, { shiftKey: true })).intent).toEqual({ kind: "type" });
  });

  it("a held Enter in the field answers nothing", () => {
    expect(cardKey(open(), key("Enter", T0 + 900, { repeat: true })).intent).toEqual(
      refused("Enter: a held key answers nothing — press it again"),
    );
  });

  it("an unarmed Enter in the field answers nothing", () => {
    expect(cardKey(open(), key("Enter", T0 + CARD_ARM_MS - 1)).intent).toEqual(
      refused("Enter: card arming — press again"),
    );
  });

  it("Alt+n, Alt+p and F1 keep their meanings in the field", () => {
    expect(cardKey(open(), key("n", T0 + 900, { altKey: true })).intent).toEqual({ kind: "other-card", delta: 1 });
    expect(cardKey(open(), key("p", T0 + 900, { altKey: true })).intent).toEqual({ kind: "other-card", delta: -1 });
    expect(cardKey(open(), key("F1", T0 + 900)).intent).toEqual({ kind: "help" });
    expect(cardKey(open(), key("n", T0 + 900, { altKey: true, repeat: true })).intent).toEqual(
      refused("Alt-n: a held key moves to no card — press it again"),
    );
  });

  it("Enter with Shift, Ctrl, Alt or Meta in the field is refused, armed or not; the field stays open", () => {
    for (const [mods, name] of [
      [{ shiftKey: true }, "Shift+Enter"],
      [{ ctrlKey: true }, "Ctrl+Enter"],
      [{ altKey: true }, "Alt+Enter"],
      [{ metaKey: true }, "Meta+Enter"],
    ] as const) {
      for (const at of [T0 + 100, ARMED + 1000]) {
        const r = cardKey(open(), key("Enter", at, mods));
        expect(r.intent, `${name} at ${at}`).toEqual(refused(`${name}: Enter alone denies with the reason`));
        expect(r.state.answering).toBeNull();
        expect(r.state.reasonOpen).toBe(true);
        expect(r.state.armFrom).toBe(Math.max(T0, at));
      }
    }
  });

  it("Shift+Tab stays refused in the field", () => {
    expect(cardKey(open(), key("Tab", T0 + 900, { shiftKey: true })).intent).toEqual(
      refused("Shift+Tab: answer or leave the card first (Esc)"),
    );
  });

  it("every other key is typed into the field and re-arms", () => {
    const typed: CardKey[] = [
      key("a", T0 + 900),
      key("1", T0 + 900),
      key("h", T0 + 900),
      key(" ", T0 + 900),
      key("Backspace", T0 + 900),
      key("Delete", T0 + 900),
      key("ArrowLeft", T0 + 900),
      key("ArrowRight", T0 + 900),
      key("Home", T0 + 900),
      key("End", T0 + 900),
      key("a", T0 + 900, { ctrlKey: true }),
      key("c", T0 + 900, { ctrlKey: true }),
      key("g", T0 + 900, { ctrlKey: true }),
      key("A", T0 + 900, { shiftKey: true }),
    ];
    for (const ev of typed) {
      const r = cardKey(open(), ev);
      expect(r.intent, ev.key).toEqual({ kind: "type" });
      expect(r.state.armFrom).toBe(T0 + 900);
      expect(r.state.reasonOpen).toBe(true);
      expect(r.state.highlight).toBeNull();
    }
  });

  it("every composing key is typed, an input method's Escape and Enter included", () => {
    for (const extra of [{ isComposing: true }, { keyCode: 229 }]) {
      for (const k of ["Escape", "Enter", "Tab", "a"]) {
        const r = cardKey(open(), key(k, T0 + 900, extra));
        expect(r.intent).toEqual({ kind: "type" });
        expect(r.state.reasonOpen).toBe(true);
      }
    }
  });
});

describe("notePaint and noteArrival", () => {
  it("notePaint marks card mode painted and arms from the paint, never earlier", () => {
    const s = enterCardMode(CARD, BROWSE, PROVIDER, T0);
    expect(s.painted).toBe(false);
    expect(notePaint(s, T0 + 40)).toMatchObject({ painted: true, armFrom: T0 + 40 });
    expect(notePaint(s, T0 - 40)).toMatchObject({ painted: true, armFrom: T0 });
  });

  it("noteArrival arms from the arrival, never earlier, and paints nothing", () => {
    const s = enterCardMode(CARD, BROWSE, PROVIDER, T0);
    expect(noteArrival(s, T0 + 70)).toMatchObject({ painted: false, armFrom: T0 + 70 });
    expect(noteArrival(s, T0 - 70).armFrom).toBe(T0);
  });
});

describe("entry and arming", () => {
  it("entry never highlights, from any origin", () => {
    for (const origin of [BROWSE, INPUT, DOCK]) {
      const s = enterCardMode(CARD, origin, PROVIDER, T0);
      expect(s.highlight).toBeNull();
      expect(s.reasonOpen).toBe(false);
      expect(s.answering).toBeNull();
      expect(s.origin).toEqual(origin);
      expect(notePaint(s, T0).highlight).toBeNull();
    }
  });

  it("each arming source moves the first armed moment to that source plus 300 ms", () => {
    const fresh = () => enterCardMode(CARD, BROWSE, PROVIDER, T0);
    const sources: Array<[string, CardModeState, number]> = [
      ["entry press", notePaint(fresh(), T0 - 5), T0],
      ["paint", notePaint(fresh(), T0 + 100), T0 + 100],
      ["arrival", noteArrival(notePaint(fresh(), T0 + 10), T0 + 200), T0 + 200],
      ["refused key", cardKey(notePaint(fresh(), T0 + 10), key("x", T0 + 250)).state, T0 + 250],
    ];
    for (const [what, s, from] of sources) {
      expect(isArmed(s, from + CARD_ARM_MS - 1), what).toBe(false);
      expect(isArmed(s, from + CARD_ARM_MS), what).toBe(true);
      expect(cardKey(s, key("1", from + CARD_ARM_MS - 1)).intent.kind, what).toBe("refuse");
      expect(cardKey(s, key("1", from + CARD_ARM_MS)).intent.kind, what).toBe("answer");
    }
  });

  it("a digit at armFrom + 299 refuses, at + 300 answers", () => {
    expect(cardKey(entered(), key("2", T0 + 299)).intent).toEqual(refused("2: card arming — press again"));
    expect(cardKey(entered(), key("2", T0 + 300)).intent).toMatchObject({ kind: "answer", button: 1 });
  });

  it("card mode never painted is never armed, however long", () => {
    const s = enterCardMode(CARD, BROWSE, PROVIDER, T0);
    expect(isArmed(s, T0 + 1e9)).toBe(false);
    expect(cardKey(s, key("2", T0 + 1e9)).intent).toEqual(refused("2: card arming — press again"));
    const lit = press(s, key("l", T0 + 10)).state;
    expect(cardKey(lit, key("Enter", T0 + 1e9)).intent).toEqual(refused("Enter: card arming — press again"));
  });
});

describe("the after-half", () => {
  it("keyCancelsAnswer clears the answer in flight and arms again from the later of its press and the key's", () => {
    const answering = cardKey(entered(), key("1", ARMED + 20)).state;
    const s = keyCancelsAnswer(answering, ARMED + 100);
    expect(s.answering).toBeNull();
    expect(s.armFrom).toBe(ARMED + 100);
    expect(keyAnswerProof(s, CARD, ARMED + 20, true, LATE)).toBeNull();
    // A key reported out of order, before the answer's press, still leaves the arming at that press.
    expect(keyCancelsAnswer(answering, ARMED).armFrom).toBe(ARMED + 20);
    // With nothing in flight it re-arms too.
    const idle = keyCancelsAnswer(entered(), ARMED + 50);
    expect(idle.answering).toBeNull();
    expect(idle.armFrom).toBe(ARMED + 50);
  });

  it("a key only the page's guard sees (Super) in the after-half cancels it, and the next digit is unarmed", () => {
    const answering = cardKey(entered(), key("1", ARMED)).state;
    const cancelled = keyCancelsAnswer(answering, ARMED + 100);
    expect(cardKey(cancelled, key("1", ARMED + 300)).intent).toEqual(refused("1: card arming — press again"));
    expect(cardKey(cancelled, key("1", ARMED + 100 + CARD_ARM_MS)).intent).toMatchObject({ kind: "answer" });
  });

  it("such a key with nothing in flight re-arms as well: a digit 100 ms after it is unarmed", () => {
    const s = keyCancelsAnswer(entered(), ARMED);
    expect(cardKey(s, key("1", ARMED + 100)).intent).toEqual(refused("1: card arming — press again"));
  });

  it("a second digit after the first's after-half began re-arms from its own press, not the first's", () => {
    const answering = cardKey(entered(), key("1", ARMED)).state;
    const r = cardKey(answering, key("2", ARMED + 350));
    expect(r.intent).toEqual(refused("2: card arming — press again"));
    expect(r.state.armFrom).toBe(ARMED + 350);
    expect(r.state.answering).toBeNull();
  });

  it("a key in the after-half clears the answer and re-arms from that key", () => {
    const answering = cardKey(entered(), key("1", ARMED)).state;
    const r = cardKey(answering, key("j", ARMED + 120));
    expect(r.state.answering).toBeNull();
    expect(r.state.armFrom).toBe(ARMED + 120);
    expect(keyAnswerProof(r.state, CARD, ARMED, true, LATE)).toBeNull();
  });

  it("an answer key in the after-half answers nothing itself", () => {
    const answering = cardKey(entered(), key("1", ARMED)).state;
    const r = cardKey(answering, key("1", ARMED + 100));
    expect(r.intent).toEqual(refused("1: card arming — press again"));
    expect(r.state.answering).toBeNull();
    expect(r.state.armFrom).toBe(ARMED + 100);
  });

  it('"1. do x" typed as one burst answers nothing and leaves the arming at its last key', () => {
    const burst = ["1", ".", " ", "d", "o", " ", "x"].map((k, i) => key(k, ARMED + i * 40));
    const r = press(entered(), ...burst);
    expect(r.state.answering).toBeNull();
    expect(r.state.armFrom).toBe(ARMED + 6 * 40);
    expect(keyAnswerProof(r.state, CARD, ARMED, true, LATE)).toBeNull();
    expect(r.intent.kind).toBe("refuse");
  });

  it("an arrival in the after-half voids the answer's proof", () => {
    const answering = cardKey(entered(), key("2", ARMED)).state;
    expect(keyAnswerProof(answering, CARD, ARMED, true, LATE)).not.toBeNull();
    expect(keyAnswerProof(noteArrival(answering, ARMED + 100), CARD, ARMED, true, LATE)).toBeNull();
  });
});

describe("each card's answer list", () => {
  it("the last answer is the one deny-like answer on every card kind today: 3 beside a rule offer, 2 elsewhere", () => {
    expect(ANSWERS_WITH_RULE.map((a) => a.kind)).toEqual(["approve", "always", "deny"]);
    expect(ANSWERS_PLAIN.map((a) => a.kind)).toEqual(["approve", "deny"]);
    for (const offer of [PROVIDER, PROVIDER_2, BROKER, ACP_NO_ALLOW]) {
      expect(answersValid(offer.answers)).toBe(true);
      const last = offer.answers.length;
      expect(denyDigit(offer.answers)).toBe(last);
      expect(offer.answers.filter((a) => a.kind === "deny")).toHaveLength(1);
      expect(offer.answers[last - 1].kind).toBe("deny");
      expect(cardKey(entered(offer), key(String(last), ARMED)).intent).toMatchObject({ kind: "answer", decision: "deny" });
    }
    expect(denyDigit(PROVIDER.answers)).toBe(3);
    expect(denyDigit(PROVIDER_2.answers)).toBe(2);
  });

  it("card mode refuses a list whose one deny-like answer is not last, with two of them, or with fewer than 2 or more than 9 answers", () => {
    const approve: CardAnswer = { kind: "approve" };
    const always: CardAnswer = { kind: "always" };
    const deny: CardAnswer = { kind: "deny" };
    const tooMany = [...Array<CardAnswer>(9).fill(approve), deny];
    for (const answers of [
      [deny, approve],
      [approve, deny, always],
      [approve, approve],
      [approve, deny, deny],
      [deny, deny],
      [approve],
      [deny],
      [],
      tooMany,
    ]) {
      expect(answersValid(answers)).toBe(false);
      expect(() => enterCardMode(CARD, BROWSE, { ...PROVIDER, answers }, T0)).toThrow();
    }
    expect(answersValid([approve, always, deny])).toBe(true);
    expect(answersValid([...Array<CardAnswer>(8).fill(approve), deny])).toBe(true);
  });

  it("a longer list takes digits up to its length, each answer as its kind", () => {
    const four: CardOffer = {
      ...PROVIDER,
      answers: [{ kind: "approve" }, { kind: "always" }, { kind: "approve" }, { kind: "deny" }],
    };
    expect(cardKey(entered(four), key("3", ARMED)).intent).toMatchObject({
      kind: "answer",
      button: 2,
      decision: "allow",
      remember: false,
    });
    expect(cardKey(entered(four), key("4", ARMED)).intent).toMatchObject({ kind: "answer", button: 3, decision: "deny" });
    expect(cardKey(entered(four), key("5", ARMED)).intent).toEqual(refused("5: this card has 4 answers"));
    expect(press(entered(four), key("$", ARMED, { shiftKey: true })).state.highlight).toBe(3);
  });
});

describe("card kinds", () => {
  it("an ACP-like card with two answers refuses 3", () => {
    const acp: CardOffer = { answers: ANSWERS_PLAIN, approvable: true, takesReason: false, noReasonFlash: "no reason" };
    expect(cardKey(entered(acp), key("3", ARMED)).intent).toEqual(refused("3: this card has 2 answers"));
    expect(cardKey(entered(acp), key("1", ARMED)).intent).toMatchObject({ kind: "answer", decision: "allow" });
  });

  it("a broker-like card refuses Tab with its own text", () => {
    expect(cardKey(entered(BROKER), key("Tab", ARMED)).intent).toEqual(refused(BROKER.noReasonFlash));
  });

  it("a reason typed as b e c a u s e: each typed, the arming follows the last letter", () => {
    let s = cardKey(entered(), key("Tab", ARMED)).state;
    let t = ARMED + 100;
    for (const ch of "because") {
      const r = cardKey(s, key(ch, t));
      expect(r.intent).toEqual({ kind: "type" });
      expect(r.state.armFrom).toBe(t);
      s = r.state;
      t += 60;
    }
    const last = t - 60;
    expect(cardKey(s, key("Enter", last + CARD_ARM_MS - 1)).intent).toEqual(refused("Enter: card arming — press again"));
    expect(cardKey(s, key("Enter", last + CARD_ARM_MS)).intent).toEqual({
      kind: "answer",
      button: 2,
      answer: { kind: "deny" },
      decision: "deny",
      remember: false,
      withReason: true,
    });
  });

  it("Escape closes the field and card mode stays", () => {
    const s = cardKey(entered(), key("Tab", ARMED)).state;
    const r = cardKey(s, key("Escape", ARMED + 500));
    expect(r.intent).toEqual({ kind: "reason", open: false });
    expect(r.state.card).toEqual(CARD);
    expect(cardKey(r.state, key("2", ARMED + 500 + CARD_ARM_MS)).intent).toMatchObject({ kind: "answer", button: 1 });
  });

  it("an IME Escape keeps the field open: with keyCode 229 and with isComposing", () => {
    const s = cardKey(entered(), key("Tab", ARMED)).state;
    for (const extra of [{ keyCode: 229 }, { isComposing: true }]) {
      const r = cardKey(s, key("Escape", ARMED + 500, extra));
      expect(r.intent).toEqual({ kind: "type" });
      expect(r.state.reasonOpen).toBe(true);
    }
  });

  it("Tab closes the field", () => {
    const s = cardKey(entered(), key("Tab", ARMED)).state;
    expect(cardKey(s, key("Tab", ARMED + 500)).intent).toEqual({ kind: "reason", open: false });
  });

  it("a composing a in the field is typed", () => {
    const s = cardKey(entered(), key("Tab", ARMED)).state;
    expect(cardKey(s, key("a", ARMED + 500, { isComposing: true })).intent).toEqual({ kind: "type" });
  });
});

describe("keyAnswerProof", () => {
  const answering = () => cardKey(entered(), key("1", ARMED)).state;

  it("is given only once the after-half is over, on the keydowns' clock", () => {
    expect(keyAnswerProof(answering(), CARD, ARMED, true, ARMED)).toBeNull();
    expect(keyAnswerProof(answering(), CARD, ARMED, true, ARMED + TYPING_GUARD_MS - 1)).toBeNull();
    expect(keyAnswerProof(answering(), CARD, ARMED, true, ARMED + TYPING_GUARD_MS)).not.toBeNull();
  });

  it("names the answer in flight: the button and what it means; a deny carries the field's text, a grant none", () => {
    const by = (digit: string) => {
      const s = cardKey(entered(), key(digit, ARMED)).state;
      return keyAnswerProof(s, CARD, ARMED, true, LATE, "ignored");
    };
    expect(by("1")).toMatchObject({ button: 0, answer: { kind: "approve" }, decision: "allow", remember: false, withReason: false, reason: null });
    expect(by("3")).toMatchObject({ button: 2, answer: { kind: "deny" }, decision: "deny", remember: false, withReason: false, reason: "ignored" });
    const shut = cardKey(entered(), key("3", ARMED)).state;
    expect(keyAnswerProof(shut, CARD, ARMED, true, LATE)?.reason).toBe("");
    expect(by("2")).toMatchObject({ button: 1, answer: { kind: "always" }, decision: "allow", remember: true, withReason: false, reason: null });
  });

  it("carries the reason for a deny from the reason field", () => {
    const open = cardKey(entered(), key("Tab", T0)).state;
    const s = cardKey(open, key("Enter", ARMED)).state;
    expect(keyAnswerProof(s, CARD, ARMED, true, LATE, "too broad")).toMatchObject({
      button: 2,
      decision: "deny",
      remember: false,
      withReason: true,
      reason: "too broad",
    });
    expect(keyAnswerProof(s, CARD, ARMED, true, LATE)?.reason).toBe("");
  });

  it("is given for the live card's answer in flight, armed, still waiting", () => {
    const proof = keyAnswerProof(answering(), CARD, ARMED, true, LATE);
    expect(proof).not.toBeNull();
    expect(proof?.card).toEqual(CARD);
    expect(proof?.pressedAt).toBe(ARMED);
  });

  it("is null for another card", () => {
    expect(keyAnswerProof(answering(), { permissionId: "perm-2", tab: 1 }, ARMED, true, LATE)).toBeNull();
    expect(keyAnswerProof(answering(), { permissionId: "perm-1", tab: 2 }, ARMED, true, LATE)).toBeNull();
  });

  it("is null for a stale press", () => {
    expect(keyAnswerProof(answering(), CARD, ARMED - 1, true, LATE)).toBeNull();
    expect(keyAnswerProof(answering(), CARD, ARMED + 1, true, LATE)).toBeNull();
  });

  it("is null with no answer in flight", () => {
    expect(keyAnswerProof(entered(), CARD, ARMED, true, LATE)).toBeNull();
  });

  it("is null when unarmed at the press", () => {
    const s: CardModeState = { ...entered(), answering: { button: 0, pressedAt: T0 + 10, withReason: false } };
    expect(keyAnswerProof(s, CARD, T0 + 10, true, LATE)).toBeNull();
    const unpainted: CardModeState = { ...answering(), painted: false };
    expect(keyAnswerProof(unpainted, CARD, ARMED, true, LATE)).toBeNull();
  });

  it("is null when the card no longer waits", () => {
    expect(keyAnswerProof(answering(), CARD, ARMED, false, LATE)).toBeNull();
  });

  it("is null with no card mode", () => {
    expect(keyAnswerProof(null, CARD, ARMED, true, LATE)).toBeNull();
  });
});

describe("pointerAnswerProof", () => {
  it("is null for a click Enter or Space synthesised (detail 0)", () => {
    expect(pointerAnswerProof({ detail: 0 })).toBeNull();
    expect(pointerAnswerProof({ detail: -1 })).toBeNull();
    expect(pointerAnswerProof({ detail: Number.NaN })).toBeNull();
  });

  it("is given for a real press (detail 1 or more)", () => {
    expect(pointerAnswerProof({ detail: 1 })).not.toBeNull();
    expect(pointerAnswerProof({ detail: 2 })).not.toBeNull();
  });
});

describe("proofAnswers: a call that disagrees with its key proof sends nothing", () => {
  const RULED: CardOffer = { ...PROVIDER, rule: "Bash(git status *)" };
  const proofOf = (digit: string, offer: CardOffer = RULED) => {
    const answering = cardKey(entered(offer), key(digit, ARMED)).state;
    const proof = keyAnswerProof(answering, CARD, ARMED, true, LATE);
    expect(proof).not.toBeNull();
    return proof!;
  };

  it("the proof's own card, tab, decision and remember answer", () => {
    expect(proofAnswers(proofOf("1"), "perm-1", 1, "allow", false, "Bash(git status *)")).toBe(true);
    expect(proofAnswers(proofOf("3"), "perm-1", 1, "deny", false, null)).toBe(true);
    expect(proofAnswers(proofOf("2"), "perm-1", 1, "allow", true, "Bash(git status *)")).toBe(true);
  });

  it("another card, another tab, another decision or another remember do not", () => {
    const approve = proofOf("1");
    expect(proofAnswers(approve, "perm-2", 1, "allow", false, null)).toBe(false);
    expect(proofAnswers(approve, "perm-1", 2, "allow", false, null)).toBe(false);
    expect(proofAnswers(approve, "perm-1", 1, "deny", false, null)).toBe(false);
    expect(proofAnswers(approve, "perm-1", 1, "allow", true, "Bash(git status *)")).toBe(false);
    expect(proofAnswers(proofOf("2"), "perm-1", 1, "allow", false, "Bash(git status *)")).toBe(false);
  });

  it("a lasting grant answers only under the rule the card showed at the press", () => {
    const always = proofOf("2");
    expect(always.rule).toBe("Bash(git status *)");
    expect(proofAnswers(always, "perm-1", 1, "allow", true, "Bash(git *)")).toBe(false);
    expect(proofAnswers(always, "perm-1", 1, "allow", true, null)).toBe(false);
    // An offer that named no rule text proves no grant of any rule.
    expect(proofAnswers(proofOf("2", PROVIDER), "perm-1", 1, "allow", true, "Bash(git status *)")).toBe(false);
    // Other answers carry no rule and need none.
    expect(proofOf("1").rule).toBeNull();
    expect(proofOf("3").rule).toBeNull();
  });
});
