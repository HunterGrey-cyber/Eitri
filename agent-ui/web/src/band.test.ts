import { describe, expect, it } from "vitest";
import { remoteLinkLine, phoneTitle, backgroundSegment, bandLayout, closingPath, cutFlash, bandLayoutSteady, liveTasks, taskWords, tasksLine, textWidth, usageSegment, RETURN_COOLDOWN_MS, RETURN_MARGIN, type BandFacts, type LayoutHold } from "./band";
import { applyEvent, initialState } from "./reducer";
import { revealedPathText } from "./revealHidden";
import {
  ANSWER_CANCELLED_FLASH,
  ANSWER_INTERRUPTED_FLASH,
  BROKER_NO_REASON_FLASH,
  CARD_CHANGED_FLASH,
  CTRL_G_IN_CARD_FLASH,
  CTRL_Y_GONE_FLASH,
  ENTER_ALONE_FLASH,
  ESC_DOES_NOT_INTERRUPT_FLASH,
  QUESTION_WAITS_FLASH,
  REASON_ENTER_OUTSIDE_FLASH,
  REPLAYED_KEY_FLASH,
  SHIFT_TAB_IN_CARD_FLASH,
  SHORT_FLASH,
  agentNoReasonFlash,
  cardArmingFlash,
  cardJumpGoneFlash,
  cardKeyGoneFlash,
  heldAnswerFlash,
  heldCardChordFlash,
  lettersNeverAnswerFlash,
  modifiedReasonEnterFlash,
  noButtonPickedFlash,
  noSuchAnswerFlash,
  notAKeyHereFlash,
  notApprovableFlash,
  pathPickInCardFlash,
  shiftedDigitFlash,
} from "./app/flashes";

/* Two long flashes with short forms: `a` where no card is, and `a` on a card (card mode's pointer). */
const A_ELSEWHERE_FLASH = notAKeyHereFlash("a");
const A_ON_CARD_FLASH = cardKeyGoneFlash("a");
import type { BackgroundTask, BackgroundTaskRecord, TaskKind, TokenUsage } from "./types";

const running: BandFacts = { mode: "input", pill: "⏵⏵ auto", showcmd: null, message: null, prompt: null, warn: null,
  unread: "↓3", cards: 1, queued: 1, context: { file: "eitri.zsh", lines: [3, 9] }, position: "14/30", model: "sonnet-5", usage: null };
const idle: BandFacts = { ...running, mode: "browse", unread: null, cards: 0, queued: 0, context: null };
const ids = (w: number, f: BandFacts) => bandLayout(f, w, 7.2).map((s) => s.id);

describe("card mode's band", () => {
  const card = (facts: BandFacts, width = 2000) => bandLayout(facts, width, 7.2).find((s) => s.id === "card");
  /** BROWSE in card mode on the first of two cards, with the running band's figures, counts and unread mark. */
  const holding = (over: Partial<NonNullable<BandFacts["cardMode"]>> = {}, more: Partial<BandFacts> = {}): BandFacts => ({
    ...running, mode: "browse", cards: 2, cardMode: { index: 1, total: 2, answers: 3, reason: true, ...over }, ...more,
  });

  it.each([2, 3, 9])("names the held card and its own %i answers", (answers) => {
    expect(card({ ...running, cards: 4, cardMode: { index: 2, total: 4, answers, reason: true } })).toEqual({
      id: "card", text: `card 2/4 · 1-${answers} answer · h/l Enter · Tab reason · Esc leaves`, side: "left",
    });
  });

  it("names Tab only on a card whose deny takes a reason", () => {
    expect(card(holding({ answers: 2, reason: false }))?.text).toBe("card 1/2 · 1-2 answer · h/l Enter · Esc leaves");
    expect(card(holding({ answers: 2, reason: false }), 360)?.text).toBe("card 1/2 · 1-2 · Esc");
  });

  it("with the reason field open, says what Enter and Esc do there (the digits are text in it)", () => {
    expect(card(holding({ reasonOpen: true }))?.text).toBe("card 1/2 · Enter denies with this reason · Esc back, text kept");
    expect(card(holding({ reasonOpen: true }), 360)?.text).toBe("card 1/2 · Enter denies");
  });

  it("whole on a wide panel; the keys by name at 520 and 360 px, beside the mode's word, the pill and the unread mark", () => {
    expect(card(holding(), 900)?.text).toBe("card 1/2 · 1-3 answer · h/l Enter · Tab reason · Esc leaves");
    for (const width of [520, 360]) {
      const segs = bandLayout(holding(), width, 7.2);
      expect(segs.map((s) => s.id), `${width}`).toEqual(["mode", "pill", "card", "unread"]);
      expect(segs.find((s) => s.id === "card")!.text, `${width}`).toBe("card 1/2 · 1-3 · Tab · Esc");
      expect(segs.find((s) => s.id === "mode")!.text, `${width}`).toBe("CARD");
    }
  });

  it("gives way to the card's place alone beside a flash, so the flash says why at 360 px", () => {
    const refusal = cardArmingFlash("1");
    const wide = bandLayout(holding({}, { message: refusal }), 2000, 7.2);
    expect(wide.find((s) => s.id === "card")!.text).toBe("card 1/2 · 1-3 answer · h/l Enter · Tab reason · Esc leaves");
    expect(wide.find((s) => s.id === "message")!.text).toBe(refusal);
    const narrow = bandLayout(holding({}, { message: refusal }), 360, 7.2);
    expect(narrow.map((s) => s.id)).toEqual(["mode", "pill", "card", "message", "unread"]);
    expect(narrow.find((s) => s.id === "card")!.text).toBe("card 1/2");
    expect(narrow.find((s) => s.id === "message")).toMatchObject({ text: "1: not armed yet", full: refusal });
    expect(narrow.find((s) => s.id === "message")!.cut).toBeFalsy();
  });

  it("offers no way in out of card mode: the activity line says how card mode is entered", () => {
    for (const mode of ["browse", "input", "caret", "visual", "vline", "hint"] as const) {
      expect(card({ ...running, mode, cards: 2 }), mode).toBeUndefined();
    }
  });

  it("reads CARD in the mode block while card mode holds, whichever mode it came from", () => {
    for (const mode of ["browse", "input"] as const) {
      const segs = bandLayout(holding({}, { mode }), 2000, 7.2);
      expect(segs.find((s) => s.id === "mode")!.text).toBe("CARD");
    }
    expect(bandLayout({ ...idle, cards: 1 }, 2000, 7.2).find((s) => s.id === "mode")!.text).toBe("BROWSE");
  });

  it.each([322, 360])("at %i px a refusal in card mode is shown whole: it takes the mode's word and the card's place first", (width) => {
    for (const refusal of [lettersNeverAnswerFlash("x", 3), cardArmingFlash("1"), ANSWER_CANCELLED_FLASH, SHIFT_TAB_IN_CARD_FLASH, CTRL_Y_GONE_FLASH, A_ON_CARD_FLASH]) {
      const segs = bandLayout(holding({}, { message: refusal }), width, 7.2);
      const message = segs.find((s) => s.id === "message")!;
      expect(message.cut, `${width}: ${refusal}`).toBeFalsy();
      expect([refusal, SHORT_FLASH.get(refusal)], `${width}: ${refusal}`).toContain(message.text);
    }
  });

  it("offers no card key with only questions waiting, or under a confirmation", () => {
    expect(card({ ...idle, questions: 2 })).toBeUndefined();
    expect(card({ ...running, cardMode: { index: 1, total: 1, answers: 2, reason: true }, prompt: "close? (y/n)" })).toBeUndefined();
  });

  it("keeps the held card's instructions when figures give way, and offers nothing on leaving", () => {
    const held = { ...running, cardMode: { index: 1, total: 1, answers: 2, reason: true } };
    expect(bandLayout(held, 600, 7.2).find((s) => s.id === "card")?.text).toBe(card(held)?.text);
    expect(card({ ...held, cardMode: null })).toBeUndefined();
    expect(card({ ...idle, cardMode: null })).toBeUndefined();
  });
});

/* Every card-mode flash, as card mode and the page build them, so each has registered its short form before the
   40-column check below reads the whole table. */
const CARD_MODE_FLASHES = [
  REPLAYED_KEY_FLASH, SHIFT_TAB_IN_CARD_FLASH, CTRL_G_IN_CARD_FLASH, ANSWER_CANCELLED_FLASH, ANSWER_INTERRUPTED_FLASH,
  CARD_CHANGED_FLASH, QUESTION_WAITS_FLASH, ENTER_ALONE_FLASH, REASON_ENTER_OUTSIDE_FLASH, BROKER_NO_REASON_FLASH,
  CTRL_Y_GONE_FLASH, agentNoReasonFlash("Codex", 2), agentNoReasonFlash("goose", 2), agentNoReasonFlash("Codex", 3), cardJumpGoneFlash("]p"), cardJumpGoneFlash("[p"),
  ...["1", "3", "9"].map(shiftedDigitFlash), ...["1", "Enter"].map(heldAnswerFlash), ...["1", "Enter"].map(cardArmingFlash),
  noSuchAnswerFlash("4", 3), noSuchAnswerFlash("3", 2), noButtonPickedFlash(2), noButtonPickedFlash(3),
  ...["Shift+Enter", "Ctrl+Enter", "Alt+Enter"].map(modifiedReasonEnterFlash), heldCardChordFlash("Alt-n"), heldCardChordFlash("Alt-p"),
  ...["a", "D", "?", "Space", "Ctrl+x", "F5"].map((key) => lettersNeverAnswerFlash(key, 3)),
  ...["1", "3", "Enter"].flatMap((key) => [notApprovableFlash(key, 2), notApprovableFlash(key, 3)]),
  pathPickInCardFlash("gd"), pathPickInCardFlash("gf"),
];

describe("the phone mark", () => {
  it("names a partial sandbox in the phone tooltip", () => {
    expect(phoneTitle("partial")).toContain("sandbox: partial");
    expect(phoneTitle("full")).not.toContain("partial");
    expect(phoneTitle()).not.toContain("partial");
  });
  it("band_question_mark_names_grant_and_host: the mark's title names each phone, its grant and the Host it came through", () => {
    const title = phoneTitle("full", [
      { device: "d1", name: "Pixel\u202e9", grant: "watch+steer", host: "127.0.0.1:7341", since: 5 },
      { device: "d2", name: "Tablet", grant: "watch", host: "desk.tail1\u200b.ts.net", since: 9 },
    ]);
    expect(title).toContain("Pixel⟨U+202E⟩9 (watch+steer, through 127.0.0.1:7341)");
    expect(title).toContain("Tablet (watch, through desk.tail1⟨U+200B⟩.ts.net)");
    expect(title).not.toContain("\u202e");
    expect(title).not.toContain("\u200b");
    expect(phoneTitle("partial", [])).toContain("sandbox: partial");
  });
  it("phone_mark_counts_connected_devices", () => {
    const wide = bandLayout({ ...running, phones: 2 }, 900, 7.2);
    expect(wide.find((s) => s.id === "phone")).toEqual({ id: "phone", text: "☎2", side: "left" });
    expect(bandLayout({ ...running, phones: 1 }, 900, 7.2).find((s) => s.id === "phone")?.text).toBe("☎1");
    // Beside the other left-hand counts, ahead of the message.
    const order = wide.map((s) => s.id);
    expect(order.indexOf("phone")).toBeGreaterThan(order.indexOf("queue"));
  });
  it("phone_mark_absent_when_none", () => {
    expect(ids(900, running)).not.toContain("phone");
    expect(ids(900, { ...running, phones: 0 })).not.toContain("phone");
    expect(ids(900, { ...running, phones: null })).not.toContain("phone");
  });
  it("phone_mark_outlasts_every_other_segment", () => {
    const facts: BandFacts = { ...running, phones: 3, background: 1, recovery: { path: "/a/b.txt", more: 0 }, ending: { kind: "failed", reason: "x", apiErrorStatus: null, message: "m" } };
    for (let width = 1; width <= 900; width++) expect(ids(width, facts)).toContain("phone");
    // Before the first measurement, and under a prompt, which take the band whole for themselves.
    expect(bandLayout({ ...running, phones: 3 }, 0, 0).map((s) => s.id)).toEqual(["mode", "pill", "phone"]);
    expect(bandLayout({ ...running, phones: 3, prompt: "close the window? (y/n)" }, 900, 7.2).map((s) => s.id)).toEqual(["mode", "prompt", "phone"]);
  });
});

describe("the band degrades by priority", () => {
  it("following_shows_and_goes_when_narrow", () => {
    const facts = { ...idle, queued: 1, background: 1, following: true };
    const wide = bandLayout(facts, 900, 7.2);
    expect(wide.find((s) => s.id === "following")).toEqual({ id: "following", text: "following", side: "left" });
    expect(wide.map((s) => s.id).indexOf("following")).toBe(wide.map((s) => s.id).indexOf("background") + 1);
    expect(ids(900, { ...facts, following: false })).not.toContain("following");
    expect(ids(120, facts)).not.toContain("following");
    const intermediate: string[][] = [];
    for (let width = 120; width <= 900; width++) {
      const shown = ids(width, facts);
      if (shown.includes("following")) expect(shown).toContain("background");
      if (shown.includes("queue")) expect(shown).toContain("following");
      if (shown.includes("following") && !shown.includes("queue")) intermediate.push(shown);
    }
    expect(intermediate.length).toBeGreaterThan(0);
  });
  it("fits the mockup's two states whole", () => {
    expect(ids(520, running)).toEqual(["mode", "pill", "cards", "queue", "context", "model", "position", "unread"]);
    expect(ids(360, idle)).toEqual(["mode", "pill", "model", "position"]);
  });
  it("drops model, then position, then context, and never mode, pill, warn or unread", () => {
    expect(ids(300, running)).not.toContain("model");
    const tiny = bandLayout({ ...running, warn: "skew" }, 120, 7.2);
    expect(tiny.map((s) => s.id)).toEqual(expect.arrayContaining(["mode", "pill", "warn", "unread"]));
    expect(tiny.find((s) => s.id === "mode")!.text).toBe("I");
  });
  it("shortens context to the file before dropping it", () => {
    const seg = bandLayout(running, 330, 7.2).find((s) => s.id === "context");
    expect(seg?.text === "⧉ eitri.zsh" || seg === undefined).toBe(true);
    expect(bandLayout(running, 520, 7.2).find((s) => s.id === "context")!.text).toBe("⧉ eitri.zsh:3-9");
  });
  it("a link that is not attached replaces the context segment with its text", () => {
    const link = { state: "detached" as const, text: "editor detached: run :EitriPanel to attach again" };
    const segs = bandLayout({ ...running, link }, 900, 7.2);
    expect(segs.map((s) => s.id)).toEqual(["mode", "pill", "cards", "queue", "link", "model", "position", "unread"]);
    expect(segs.find((s) => s.id === "link")).toEqual({ id: "link", text: link.text, side: "right" });
  });
  it("shows the context when the editor is attached, and nothing changes without a link", () => {
    const attached = { state: "attached" as const, text: "" };
    expect(ids(520, { ...running, link: attached })).toEqual(ids(520, running));
    expect(ids(520, { ...running, link: null })).toEqual(ids(520, running));
    expect(ids(520, { ...running, link: attached })).toContain("context");
  });
  it("drops the link text with the context when the band is too narrow", () => {
    const link = { state: "none" as const, text: "no editor attached: run :EitriPanel in nvim" };
    expect(ids(330, { ...running, link })).not.toContain("link");
  });
  it("shows the review pointer, worded as a count of files that changed and the key that opens them", () => {
    const seg = (files: number) => bandLayout({ ...idle, review: { files } }, 900, 7.2).find((s) => s.id === "review");
    expect(seg(3)).toEqual({ id: "review", text: "3 files changed · c to review", side: "right" });
    expect(seg(1)?.text).toBe("1 file changed · c to review");
    expect(seg(0)).toBeUndefined();
    expect(bandLayout({ ...idle, review: null }, 900, 7.2).map((s) => s.id)).not.toContain("review");
  });
  it("offers only the key when the review pointer is key-only", () => {
    const seg = (files: number) => bandLayout({ ...idle, review: { files, keyOnly: true } }, 900, 7.2).find((s) => s.id === "review");
    expect(seg(2)).toEqual({ id: "review", text: "c to review", side: "right" });
    expect(seg(1)?.text).toBe("c to review");
    expect(seg(0)).toBeUndefined();
  });
  it("names an interrupted revert whatever the review pointer says, with the key that opens it", () => {
    const seg = (more: number) => bandLayout({ ...idle, review: null, recovery: { path: "core/src/x.rs", more } }, 900, 7.2).find((s) => s.id === "recovery");
    expect(seg(0)).toEqual({ id: "recovery", text: "an interrupted revert left core/src/x.rs · c to review", side: "left" });
    expect(seg(2)?.text).toBe("an interrupted revert left core/src/x.rs and 2 more · c to review");
    expect(bandLayout({ ...idle, recovery: null }, 900, 7.2).map((s) => s.id)).not.toContain("recovery");
    expect(bandLayout({ ...idle, review: { files: 2 }, recovery: { path: "a", more: 0 } }, 900, 7.2).map((s) => s.id)).toEqual(expect.arrayContaining(["review", "recovery"]));
  });
  it("keeps the recovery notice after the figures and pointers, shortens it to the file's name, and never over a prompt", () => {
    const facts: BandFacts = { ...idle, review: { files: 3 }, recovery: { path: "core/src/deeply/nested/x.rs", more: 0 } };
    // Narrow enough that the review pointer, the model and the position are gone: the notice is still there.
    for (let width = 140; width <= 900; width += 10) {
      const shown = bandLayout(facts, width, 7.2).map((s) => s.id);
      if (shown.includes("review") || shown.includes("model")) expect(shown, `width ${width}`).toContain("recovery");
    }
    const texts = new Set<string>();
    for (let width = 100; width <= 900; width += 5) {
      const text = bandLayout(facts, width, 7.2).find((s) => s.id === "recovery")?.text;
      if (text !== undefined) texts.add(text);
    }
    expect([...texts].sort()).toEqual([
      "an interrupted revert left core/src/deeply/nested/x.rs · c to review",
      "an interrupted revert left x.rs · c to review",
    ]);
    expect(ids(900, { ...facts, prompt: "close? (y/n)" })).toEqual(["mode", "prompt"]);
  });
  it("shortens the review pointer to its key, and keeps it past the effort, the model and the position", () => {
    const facts: BandFacts = { ...idle, review: { files: 3 }, effort: "high" };
    expect(ids(900, facts)).toContain("review");
    const texts = new Set<string>();
    for (let width = 100; width < 900; width += 5) {
      const segs = bandLayout(facts, width, 7.2);
      const shown = segs.map((s) => s.id);
      const review = segs.find((s) => s.id === "review");
      if (review !== undefined) texts.add(review.text);
      // The figures go first: while any of them is still there, so is the pointer.
      for (const figure of ["effort", "model", "position"] as const) {
        if (shown.includes(figure)) expect(shown, `width ${width}`).toContain("review");
      }
      // Its short form comes before any figure goes.
      if (review?.text === "3 files changed · c to review") expect(shown, `width ${width}`).toEqual(expect.arrayContaining(["effort", "model", "position"]));
    }
    expect([...texts].sort()).toEqual(["3 files changed · c to review", "c to review"]);
    expect(ids(100, facts)).not.toContain("review");
    expect(ids(900, { ...facts, prompt: "close? (y/n)" })).toEqual(["mode", "prompt"]);
  });
  it("keeps the pointer in a narrow chat whose bypass pill fills the band, at both levels", () => {
    // The chat column of the newcomer-pointers scenario: 600 px of band at 11.4 px a cell, bypass on.
    const facts: BandFacts = { ...idle, pill: "⏵⏵ bypass permissions on", model: "opus-5-5", position: "4/4" };
    for (const review of [{ files: 1, keyOnly: true }, { files: 1 }]) {
      const segs = bandLayout({ ...facts, review }, 600, 11.4);
      expect(segs.find((s) => s.id === "review")?.text, JSON.stringify(review)).toBe("c to review");
      expect(segs.map((s) => s.id)).toEqual(expect.arrayContaining(["mode", "pill"]));
    }
  });
  it("puts the effort right after the model, and shows none it was not told", () => {
    const facts: BandFacts = { ...idle, effort: "xhigh" };
    const wide = bandLayout(facts, 900, 7.2);
    expect(wide.map((s) => s.id)).toEqual(["mode", "pill", "model", "effort", "position"]);
    expect(wide.find((s) => s.id === "effort")).toMatchObject({ text: "· xhigh", side: "right" });
    expect(ids(900, idle)).not.toContain("effort");
    expect(ids(900, { ...idle, effort: null })).not.toContain("effort");
    expect(ids(900, { ...idle, model: null, effort: "xhigh" })).not.toContain("effort");
  });
  it("drops the effort before the model and the review pointer, and never leaves it alone", () => {
    const facts: BandFacts = { ...idle, effort: "xhigh", review: { files: 3 } };
    expect(ids(900, facts)).toContain("effort");
    const widths = Array.from({ length: 80 }, (_, i) => 100 + i * 10);
    const shown = widths.map((width) => bandLayout(facts, width, 7.2).map((s) => s.id));
    for (const [i, row] of shown.entries()) {
      if (row.includes("effort")) expect(row, `width ${widths[i]}`).toContain("model");
      if (row.includes("effort")) expect(row, `width ${widths[i]}`).toContain("review");
    }
    expect(shown.some((row) => row.includes("model") && !row.includes("effort")), "a width keeps the model alone").toBe(true);
    expect(shown.some((row) => row.includes("review") && !row.includes("effort")), "a width keeps the pointer").toBe(true);
    // Its width counts: wide enough for the band without it, too narrow with it.
    expect(bandLayout({ ...idle, effort: "xhigh" }, 300, 7.2).map((s) => s.id)).not.toContain("effort");
  });
  it("draws a hidden character in the effort as its escape", () => {
    const seg = bandLayout({ ...idle, effort: "hi\u202egh" }, 900, 7.2).find((s) => s.id === "effort")!;
    expect(seg.text).not.toContain("\u202e");
  });
  it("a y/n prompt takes everything right of the mode", () => {
    expect(ids(520, { ...running, prompt: "close 2 \"docs\"? (y/n)" })).toEqual(["mode", "prompt"]);
  });
  it("shows mode and pill before the width is known", () => {
    expect(ids(0, running)).toEqual(["mode", "pill"]);
  });
  it("names CARET's own mode word and narrow letter", () => {
    const caret: BandFacts = { ...idle, mode: "caret" };
    expect(bandLayout(caret, 520, 7.2).find((s) => s.id === "mode")!.text).toBe("CARET");
    const tiny = bandLayout({ ...caret, warn: "skew" }, 120, 7.2);
    expect(tiny.find((s) => s.id === "mode")!.text).toBe("C");
  });
  it("counts East Asian wide characters twice", () => {
    expect(textWidth("zsh 补全")).toBe(8);
    // 8 + 9 + 31 (the wide name) + 10 + 7 = 65 > 50: model and position go, the context fits after.
    expect(ids(360, { ...idle, context: { file: "说明文档非常长的文件名称.md", lines: null } })).toEqual(["mode", "pill", "context"]);
  });
});

/** A WebKit zoom of 1.5 measures 520 logical px as 346 CSS px: a 47-column budget at 7.2 px per
 *  character. A y/n prompt must keep what `y` does and its own `(y/n)`, so it wraps; a flash is one row, cut. */
describe("a y/n prompt is never truncated; a flash is one row", () => {
  const NARROW_BUDGET_WIDTH = 340; // floor(340 / 7.2) === 47
  const R06_PROMPT = "Switch to bypass and approve the 1 waiting card? (y/n)";
  const EMPTY_TAB_PROMPT = "Switch to bypass? New sessions in this window start in bypass too (y/n)";
  const D11_FLASH = "y must be pressed on its own to enter bypass — Shift+Tab to ask again";
  /** Cells the laid-out band takes, as `bandLayout` counts them (text, one cell of padding each side, the mode's arrow). */
  const cellsOf = (segs: ReturnType<typeof bandLayout>) =>
    segs.reduce((n, x) => n + Math.max(textWidth(x.text), x.cells ?? 0) + 2 + (x.id === "mode" ? 1 : 0), 0);

  it("keeps the bypass prompt whole, (y/n) included, at a 47-column budget", () => {
    const segs = bandLayout({ ...idle, prompt: R06_PROMPT }, NARROW_BUDGET_WIDTH, 7.2);
    const prompt = segs.find((s) => s.id === "prompt")!;
    expect(prompt.text).toBe(R06_PROMPT);
    expect(prompt.text).not.toContain("…");
    expect(prompt.text).toContain("(y/n)");
    expect(prompt.wraps).toBe(true); // it did not fit whole -- StatusBand must wrap it
  });

  it("keeps the empty-tab bypass prompt whole at the same budget", () => {
    const segs = bandLayout({ ...idle, prompt: EMPTY_TAB_PROMPT }, NARROW_BUDGET_WIDTH, 7.2);
    const prompt = segs.find((s) => s.id === "prompt")!;
    expect(prompt.text).toBe(EMPTY_TAB_PROMPT);
    expect(prompt.text).not.toContain("…");
    expect(prompt.text).toContain("(y/n)");
  });

  it("no flash ever wraps: too wide for the row, it is cut at the row's end, its start kept", () => {
    const segs = bandLayout({ ...idle, message: D11_FLASH }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.wraps).toBeFalsy();
    expect(message.cut).toBe(true);
    expect(message.text).toBe(D11_FLASH); // the engine's ellipsis ends it where the row ends
    expect(message.full).toBe(D11_FLASH);
    // Nothing else had to give way to it: mode and pill keep their words.
    expect(segs.map((s) => s.id)).toEqual(["mode", "pill", "message"]);
  });

  /* At about 50 cells wide, BROWSE, `⏵⏵ auto`, `↓`, and a no-card flash must fit on one row.
     A wrapping flash would push the unread indicator onto another row. */
  it("the 'not a key here' flash at 50 cells wide: one row, its short form, the ↓ kept", () => {
    const facts: BandFacts = { ...idle, pill: "⏵⏵ auto", unread: "↓", message: A_ELSEWHERE_FLASH };
    const segs = bandLayout(facts, 50 * 7.2, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.wraps).toBeFalsy();
    expect(message.cut).toBeFalsy();
    expect(message.text).toBe(SHORT_FLASH.get(A_ELSEWHERE_FLASH));
    expect(message.full).toBe(A_ELSEWHERE_FLASH);
    expect(segs.find((s) => s.id === "unread")!.text).toBe("↓");
    expect(segs.find((s) => s.id === "mode")!.text).toBe("BROWSE");
    expect(cellsOf(segs)).toBeLessThanOrEqual(50);
    // With room, the whole sentence.
    expect(bandLayout(facts, 80 * 7.2, 7.2).find((s) => s.id === "message")!.text).toBe(A_ELSEWHERE_FLASH);
  });

  it("every width from 12 to 120 columns keeps every flash on one row: it fits, or it is marked cut", () => {
    const path = "gf: 2 of 5: src/components/very/deep/directory/StatusBand.tsx";
    for (let cols = 12; cols <= 120; cols++) {
      for (const message of [D11_FLASH, A_ELSEWHERE_FLASH, A_ON_CARD_FLASH, path]) {
        const segs = bandLayout({ ...idle, pill: "⏵⏵ manual", unread: "↓3", message }, cols * 7.2, 7.2);
        const shown = segs.find((s) => s.id === "message")!;
        expect(segs.some((s) => s.wraps === true), `${cols} columns: ${message}`).toBe(false);
        expect(shown.cut === true || cellsOf(segs) <= cols, `${cols} columns: ${message}`).toBe(true);
        expect(segs.find((s) => s.id === "unread")!.text).toBe("↓3");
        // A path keeps its file's name wherever the cut leaves room for one.
        if (message === path && shown.cut && shown.text !== path && textWidth(shown.text) >= 20) {
          expect(shown.text.endsWith("/StatusBand.tsx"), `${cols} columns: ${shown.text}`).toBe(true);
        }
      }
    }
  });

  /* A link's address before the click that opens it: a long host's own domain is at its end, so the flash is shown
     whole (`messageLook`), on its own lines above the row as a long y/n prompt is, instead of being cut. */
  it("a flash with a look is never cut: too wide, it wraps, the address entire", () => {
    const host = "github.com-sign-in-verify-your-account-now.example.net";
    const url = `https://${host}/login — click again to open`;
    const segs = bandLayout({ ...idle, unread: "↓", message: url, messageLook: host }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(url);
    expect(message.wraps).toBe(true);
    expect(segs.find((s) => s.id === "unread")!.text).toBe("↓");
    // The same text unmarked is cut.
    const cut = bandLayout({ ...idle, message: url }, NARROW_BUDGET_WIDTH, 7.2).find((s) => s.id === "message")!;
    expect(cut.wraps).toBeFalsy();
    expect(cut.cut).toBe(true);
    // Marked whole but short enough: one row, no wrap.
    const fits = bandLayout({ ...idle, message: "https://a.example — click again to open", messageLook: "a.example" }, 80 * 7.2, 7.2);
    expect(fits.find((s) => s.id === "message")!.wraps).toBeFalsy();
  });

  /* The sentence Rust says when a tab's agent has no manual mode (`TabSet::mode_offered_to`): it arrives as a
     notice, which the band shows as its flash. At the panel's 480px it is about 100 columns of text. */
  it("cuts the 'has no manual mode' sentence beside a manual pill at 480px, keeping mode and pill", () => {
    const note =
      "gemini tabs have no manual mode (the agent's own mode is set once, at its start): tab 2 is in auto";
    const segs = bandLayout({ ...idle, pill: "⏵⏵ manual", message: note }, 480, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(note);
    expect(message.cut).toBe(true);
    expect(message.wraps).toBeFalsy();
    expect(message.full).toBe(note);
    expect(segs.find((s) => s.id === "pill")!.text).toBe("⏵⏵ manual");
    expect(segs.find((s) => s.id === "mode")!.text).toBe("BROWSE");
  });

  it("a flash that is cut leaves the mode its word: the letter would not make it fit", () => {
    const segs = bandLayout({ ...idle, message: D11_FLASH }, NARROW_BUDGET_WIDTH, 7.2);
    expect(segs.find((s) => s.id === "mode")!.text).toBe("BROWSE");
    // ...while a band with no message still shortens the mode when nothing else is left to drop.
    expect(bandLayout({ ...idle, warn: "skew" }, 120, 7.2).find((s) => s.id === "mode")!.text).toBe("B");
  });

  it("the letter still replaces the word when that is what lets a message fit", () => {
    // mode 6 + pill 7 + message 28 + 3 * 2 padding = 47 > 44; with the letter, 42 fits.
    const message = "x".repeat(28);
    const segs = bandLayout({ ...idle, message }, 44 * 7.2, 7.2);
    expect(segs.find((s) => s.id === "mode")!.text).toBe("B");
    expect(segs.find((s) => s.id === "message")!.text).toBe(message);
  });

  /* A long refusal in a narrow chat (the films' 1280 px window at zoom 1.5: a band of about 37 columns beside
     a manual pill) once wrapped over six rows and covered the composer. Too wide for the row, it gives way to its short
     form, and the mode its letter, which is what lets that fit; a wide band keeps the whole sentence. */
  it("a's card-mode pointer keeps one row in a narrow chat: its short form", () => {
    const narrow = { ...idle, pill: "⏵⏵ manual", cards: 3, message: A_ON_CARD_FLASH };
    const segs = bandLayout(narrow, 37 * 7.2, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(SHORT_FLASH.get(A_ON_CARD_FLASH));
    expect(segs.find((s) => s.id === "pill")!.text).toBe("⏵⏵ manual");
    expect(segs.find((s) => s.id === "mode")!.text).toBe("B");
    const wide = bandLayout(narrow, 160 * 7.2, 7.2);
    expect(wide.find((s) => s.id === "message")!.text).toBe(A_ON_CARD_FLASH);
    expect(wide.find((s) => s.id === "mode")!.text).toBe("BROWSE");
  });

  /* A short form or a cut drops words a person may want (the pointer's "in card mode"), so the shown segment
     keeps the whole sentence for `StatusBand`'s title; a flash shown whole carries none. */
  it("a flash shown short or cut keeps its whole text for the title, a whole one carries none", () => {
    const narrow = { ...idle, pill: "⏵⏵ manual", cards: 3, message: A_ON_CARD_FLASH };
    const short = bandLayout(narrow, 37 * 7.2, 7.2).find((s) => s.id === "message")!;
    expect(short.text).toBe(SHORT_FLASH.get(A_ON_CARD_FLASH));
    expect(short.full).toBe(A_ON_CARD_FLASH);
    expect(bandLayout(narrow, 160 * 7.2, 7.2).find((s) => s.id === "message")!.full).toBeUndefined();
    // Not even the short form fits: the short form is cut, and the title is still the whole sentence.
    const cut = bandLayout(narrow, 26 * 7.2, 7.2).find((s) => s.id === "message")!;
    expect(cut.text).toBe(SHORT_FLASH.get(A_ON_CARD_FLASH));
    expect(cut.cut).toBe(true);
    expect(cut.full).toBe(A_ON_CARD_FLASH);
  });

  it("Esc's reminder keeps one row in a narrow chat too", () => {
    const segs = bandLayout({ ...idle, pill: "⏵⏵ manual", message: ESC_DOES_NOT_INTERRUPT_FLASH }, 36 * 7.2, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe("ctrl+c interrupts");
  });

  it("the short form only where it helps: the whole sentence beside the letter first", () => {
    // `B ⏵⏵ manual Esc does not interrupt — ctrl+c does`: 4 + 11 + 38 = 53 columns fit whole with the letter.
    const esc = { ...idle, pill: "⏵⏵ manual", message: ESC_DOES_NOT_INTERRUPT_FLASH };
    const fits = bandLayout(esc, 53 * 7.2, 7.2);
    expect(fits.find((s) => s.id === "message")!.text).toBe(ESC_DOES_NOT_INTERRUPT_FLASH);
    expect(fits.find((s) => s.id === "mode")!.text).toBe("B");
  });

  it("every short form fits one row beside a manual pill at 40 columns", () => {
    // 40 columns: about the narrowest chat column a split 1280 px window gives at zoom 1.5.
    for (const full of CARD_MODE_FLASHES) expect(SHORT_FLASH.has(full), full).toBe(true);
    expect(SHORT_FLASH.size).toBeGreaterThan(CARD_MODE_FLASHES.length);
    for (const [full, short] of SHORT_FLASH) {
      expect(textWidth(short)).toBeLessThan(textWidth(full));
      const segs = bandLayout({ ...idle, pill: "⏵⏵ manual", cards: 1, message: full }, 40 * 7.2, 7.2);
      expect(segs.find((s) => s.id === "message")!.text).toBe(short);
    }
  });

  /* A notification that arrives while a question is up is not part of the question: it takes the row's one-row place
     beside the prompt, cut when it does not fit, and the prompt's text and lines never change for it. */
  it("a flash beside a prompt is cut to the row and never joins the prompt", () => {
    const notice = "Saved the session to /home/u/p/.claude/sessions/2026-11-09-a-rather-long-file-name.json";
    const short = bandLayout({ ...idle, prompt: "trust? y/n", message: notice }, NARROW_BUDGET_WIDTH, 7.2);
    expect(short.find((s) => s.id === "prompt")).toMatchObject({ text: "trust? y/n" });
    expect(short.find((s) => s.id === "prompt")!.wraps).toBeFalsy();
    const message = short.find((s) => s.id === "message")!;
    expect(message.wraps).toBeFalsy();
    expect(message.cut).toBe(true);
    expect(message.full).toBe(notice);
    // A question too long for the row goes above it; the flash stays on the row, still one row.
    const long = bandLayout({ ...idle, prompt: R06_PROMPT, message: notice }, NARROW_BUDGET_WIDTH, 7.2);
    expect(long.find((s) => s.id === "prompt")).toMatchObject({ text: R06_PROMPT, wraps: true });
    expect(long.find((s) => s.id === "message")!.wraps).toBeFalsy();
  });

  it("a short prompt still fits the row whole -- no wraps flag", () => {
    const shortPrompt = 'close 1 "docs"? (y/n)';
    const segs = bandLayout({ ...idle, prompt: shortPrompt }, NARROW_BUDGET_WIDTH, 7.2);
    const prompt = segs.find((s) => s.id === "prompt")!;
    expect(prompt.text).toBe(shortPrompt);
    expect(prompt.wraps).toBeFalsy();
  });

  it("a short flash still fits the row whole, uncut", () => {
    const segs = bandLayout({ ...idle, message: "copied 12 chars" }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe("copied 12 chars");
    expect(message.full).toBeUndefined();
    expect(message.cut).toBeUndefined();
  });
});

/** Where a flash ending in a path is cut; every other flash is left for the engine's ellipsis. */
describe("cutFlash", () => {
  it("returns a flash that fits unchanged", () => {
    expect(cutFlash("copied 12 chars", 15)).toBe("copied 12 chars");
    expect(cutFlash("copied 12 chars", 40)).toBe("copied 12 chars");
  });
  it("leaves a flash with no path at its end whole: the engine ends it at the row's edge", () => {
    expect(cutFlash("no card here — i, o, A or Ctrl+j to type", 24)).toBe("no card here — i, o, A or Ctrl+j to type");
  });
  it("keeps a path's end and loses its middle when the flash ends in a path", () => {
    const cut = cutFlash("gf: 2 of 5: src/components/very/deep/directory/StatusBand.tsx", 36);
    expect(cut).toBe("gf: 2 of 5: src/co…ry/StatusBand.tsx");
    expect(textWidth(cut)).toBeLessThanOrEqual(36);
  });
  it("counts wide characters twice and never splits a code point", () => {
    const cut = cutFlash("打开失败：src/说明文档/非常长的文件名称.md", 21);
    expect(textWidth(cut)).toBeLessThanOrEqual(21);
    expect(cut.startsWith("打开")).toBe(true);
    expect(cut.endsWith(".md")).toBe(true);
    // The name (`/` and 8 emoji, 9 cells) does not leave 4 cells of the start in a room of 9: the end keeps 5.
    expect(cutFlash("😀😀😀😀 a/😀😀😀😀😀😀😀😀", 10)).toBe("😀😀😀😀…😀😀😀😀😀");
  });
  it("budgets the whole file name when the path ends in spaces, and shows those spaces", () => {
    const spaces = "⟨U+0020⟩".repeat(5);
    const text = "gf: src/very/deep/directory/StatusBand.tsx     ";
    // Room for the name and its five spaces, drawn as escapes: all of it is kept.
    const roomy = cutFlash(text, 72);
    expect(roomy.endsWith(`/StatusBand.tsx${spaces}`)).toBe(true);
    expect(textWidth(roomy)).toBeLessThanOrEqual(72);
    // Too little room for the name: as much of the end as fits, the spaces still shown.
    const tight = cutFlash(text, 32);
    expect(tight.endsWith("⟨U+0020⟩")).toBe(true);
    expect(textWidth(tight)).toBeLessThanOrEqual(32);
    // A cut never splits an escape: after the `…` the next one starts whole.
    expect(tight.slice(tight.indexOf("…") + 1).startsWith("⟨")).toBe(true);
    // Fits whole: unchanged but for the spaces, which are shown.
    expect(cutFlash("gf: src/a.rs  ", 80)).toBe("gf: src/a.rs⟨U+0020⟩⟨U+0020⟩");
  });
  it("keeps a file name with spaces in it whole when the flash names its path", () => {
    const path = "src/my very long directory name/Status Band.tsx";
    const cut = cutFlash(`gf: 2 of 5: ${path}`, 34, path);
    expect(cut.endsWith("/Status Band.tsx")).toBe(true);
    expect(cut.startsWith("gf: 2 of 5:")).toBe(true);
    expect(textWidth(cut)).toBeLessThanOrEqual(34);
  });
  it("budgets the whole name of a path with no slash (a file at the project's root)", () => {
    const name = "StatusBandWithLongName.tsx";
    expect(cutFlash(`1 of 2: ${name}`, 31, name)).toBe(`1 of…${name}`);
    const spaced = revealedPathText("Status Band Long Name.tsx  ");
    const cut = cutFlash(`1 of 2: ${spaced}`, 48, spaced);
    expect(cut.endsWith(spaced)).toBe(true);
    expect(textWidth(cut)).toBeLessThanOrEqual(48);
  });
  it("leaves the text whole when the room is too small to keep a name", () => {
    expect(cutFlash("gf: src/a/b/c/StatusBand.tsx", 8)).toBe("gf: src/a/b/c/StatusBand.tsx");
  });
});

/** The active tab's last reported usage, right of the model, dropped before
 *  anything else when the band is narrow. */
describe("the usage segment", () => {
  const u = (cost: number, tokens: TokenUsage | null) => ({ total_cost_usd: cost, num_turns: null, tokens, model: null });
  it("formats all tokens and the cost, and says nothing before a report", () => {
    expect(usageSegment(null)).toBeNull();
    expect(usageSegment(u(0.4213, { input: 10, output: 5000, cache_creation: 200_000, cache_read: 1_000_000 }))!.text).toBe("1.2M tok $0.42");
    expect(usageSegment(u(0.004, { input: 900, output: 99, cache_creation: 0, cache_read: 0 }))!.text).toBe("999 tok <$0.01");
    expect(usageSegment(u(1.5, null))!.text).toBe("$1.50");
    expect(usageSegment(u(0.4213, { input: 10, output: 20, cache_creation: 300, cache_read: 4000 }))!.title).toContain("since this tab started or resumed");
  });
  it("sits right of the model and drops first", () => {
    const withUsage: BandFacts = { ...running, usage: { text: "1.2M tok $0.42", title: "" } };
    expect(ids(900, withUsage)).toEqual(["mode", "pill", "cards", "queue", "context", "model", "usage", "position", "unread"]);
    expect(ids(520, withUsage)).toEqual(["mode", "pill", "cards", "queue", "context", "model", "position", "unread"]);
    expect(bandLayout(withUsage, 520, 7.2).find((s) => s.id === "context")!.text).toBe("⧉ eitri.zsh:3-9");
  });
  it("is never in a y/n prompt's band, and the segment carries the text it was given", () => {
    const withUsage: BandFacts = { ...running, usage: { text: "1.2M tok $0.42", title: "t" } };
    expect(ids(900, { ...withUsage, prompt: "close 2 \"docs\"? (y/n)" })).toEqual(["mode", "prompt"]);
    expect(bandLayout(withUsage, 900, 7.2).find((s) => s.id === "usage")).toMatchObject({ text: "1.2M tok $0.42", side: "right" });
  });
  it("compacts thousands and millions to one decimal, rolling over instead of printing 1000k", () => {
    const text = (n: number) => usageSegment(u(1, { input: n, output: 0, cache_creation: 0, cache_read: 0 }))!.text;
    expect(text(0)).toBe("0 tok $1.00");
    expect(text(999)).toBe("999 tok $1.00");
    expect(text(1000)).toBe("1k tok $1.00");
    expect(text(1500)).toBe("1.5k tok $1.00");
    expect(text(999_949)).toBe("999.9k tok $1.00");
    expect(text(999_950)).toBe("1M tok $1.00");
    expect(text(999_999)).toBe("1M tok $1.00");
    expect(text(1_000_000)).toBe("1M tok $1.00");
    expect(text(12_345_678)).toBe("12.3M tok $1.00");
  });
  it("rounds a cost to cents, and shows a real but sub-cent one as under a cent, never as zero", () => {
    const cost = (c: number) => usageSegment(u(c, null))!.text;
    expect(cost(0)).toBe("$0.00");
    expect(cost(0.0001)).toBe("<$0.01");
    expect(cost(0.0049)).toBe("<$0.01");
    expect(cost(0.005)).toBe("$0.01");
    expect(cost(12.3456)).toBe("$12.35");
  });
  it("the title is the whole breakdown -- every token part, the exact cost, and what the figure is", () => {
    const { title } = usageSegment(u(0.4213, { input: 10, output: 5000, cache_creation: 200_000, cache_read: 1_000_000 }))!;
    expect(title).toBe(
      "input 10 · output 5,000 · cache write 200,000 · cache read 1,000,000 · $0.4213" +
        " — since this tab started or resumed; /clear starts it over; Claude Code's own estimate, not a bill",
    );
    // Legacy reports a cost and no tokens: only what it has.
    expect(usageSegment(u(1.5, null))!.title).toBe(
      "$1.5000 — since this tab started or resumed; /clear starts it over; Claude Code's own estimate, not a bill",
    );
  });
  it("usage_without_reported_cost_shows_no_cost", () => {
    const tokens = { input: 10, output: 5000, cache_creation: 0, cache_read: 1_000_000 };
    const unpriced = (t: TokenUsage | null) => ({ ...u(0, t), cost_reported: false });
    // Tokens alone: no dollar sign anywhere, in the text or in the title.
    const seg = usageSegment(unpriced(tokens))!;
    expect(seg.text).toBe("1M tok");
    expect(seg.text).not.toContain("$");
    expect(seg.title).not.toContain("$");
    expect(seg.title).toContain("no cost reported");
    // A placeholder that is not finite is still no reason to hide the tokens.
    expect(usageSegment({ ...unpriced(tokens), total_cost_usd: Number.NaN })!.text).toBe("1M tok");
    // Neither tokens nor a cost: nothing to say.
    expect(usageSegment(unpriced(null))).toBeNull();
    // A report that says the cost was reported, or says nothing about it, keeps the dollars.
    expect(usageSegment({ ...u(1.5, tokens), cost_reported: true })!.text).toBe("1M tok $1.50");
    expect(usageSegment(u(1.5, tokens))!.text).toBe("1M tok $1.50");
  });
  it("a cost that is not a finite number is no measurement: nothing is drawn, and nothing throws", () => {
    expect(usageSegment(u(Number.NaN, { input: 1, output: 1, cache_creation: 0, cache_read: 0 }))).toBeNull();
    expect(usageSegment(u(Number.POSITIVE_INFINITY, null))).toBeNull();
    // What a `null` cost would arrive as if a non-finite float ever crossed the wire as JSON.
    expect(usageSegment(u(null as unknown as number, null))).toBeNull();
  });
});

/** INPUT's `Ctrl+y` answers no card: with a card waiting, in INPUT, at any
 *  width, no segment advertises `Ctrl+y`. */
describe("no Ctrl+y segment", () => {
  it("is drawn in no layout, with a card waiting in INPUT", () => {
    const waiting: BandFacts = { ...running, mode: "input", cards: 2 };
    for (let w = 1400; w >= 60; w -= 5) {
      const segs = bandLayout(waiting, w, 7.2);
      expect(segs.map((s) => s.id as string), `width ${w}`).not.toContain("approve");
      for (const seg of segs) expect(seg.text, `width ${w}`).not.toContain("Ctrl+y");
    }
  });
});

/** How the latest turn ended, while it is still the latest news: left side, muted, and one of the last
 *  things a narrow band lets go. */
describe("turn ending", () => {
  const segOf = (f: BandFacts, w = 900) => bandLayout(f, w, 7.2).find((s) => s.id === "ending");
  const end = (kind: "interrupted" | "failed" | "limit_reached" | "lost", reason: string | null = null, apiErrorStatus: number | null = null) => ({
    kind,
    reason,
    apiErrorStatus,
    message: null,
  });
  it("says nothing for none, and the kind's few words otherwise, on the left", () => {
    expect(segOf(idle)).toBeUndefined();
    expect(segOf({ ...idle, ending: null })).toBeUndefined();
    expect(segOf({ ...idle, ending: end("interrupted") })).toEqual({ id: "ending", text: "interrupted", side: "left" });
    expect(segOf({ ...idle, ending: end("failed") })?.text).toBe("turn failed");
    expect(segOf({ ...idle, ending: end("limit_reached", "max_turns") })?.text).toBe("turn limit");
    expect(segOf({ ...idle, ending: end("limit_reached", "blocking_limit") })?.text).toBe("context full");
    expect(segOf({ ...idle, ending: end("lost") })?.text).toBe("turn did not finish");
  });
  it("sits after the card and queue counts, before a flash", () => {
    const facts: BandFacts = { ...running, ending: end("failed"), message: "copied" };
    expect(ids(900, facts)).toEqual(["mode", "pill", "cards", "queue", "ending", "message", "context", "model", "position", "unread"]);
  });
  it("outlasts the model, usage and context, the cards and the queue, and is never in a y/n prompt's band", () => {
    const facts: BandFacts = { ...running, ending: end("failed"), usage: { text: "1k tok $0.01", title: "" } };
    // 255, not 240: INSERT is a letter longer than INPUT, and the mode block's separator arrow takes
    // a cell of its own.
    const narrow = ids(255, facts);
    expect(narrow).toContain("ending");
    expect(narrow).not.toContain("model");
    expect(narrow).not.toContain("cards");
    expect(ids(900, { ...facts, prompt: "close? (y/n)" })).toEqual(["mode", "prompt"]);
  });
});

describe("the commands an agent left running", () => {
  it("are counted on the band only while there are any, in the singular for one", () => {
    expect(backgroundSegment(1)).toBe("1 running in background");
    expect(backgroundSegment(3)).toBe("3 running in background");
    const seg = (background: number | null | undefined) => bandLayout({ ...idle, background }, 900, 7.2).find((s) => s.id === "background");
    expect(seg(2)).toEqual({ id: "background", text: "2 running in background", side: "left" });
    expect(seg(1)!.text).toBe("1 running in background");
    expect(seg(0)).toBeUndefined();
    expect(seg(null)).toBeUndefined();
    expect(seg(undefined)).toBeUndefined();
  });
  it("is never drawn over a prompt, and outlasts the figures when the band narrows", () => {
    expect(bandLayout({ ...idle, background: 2, prompt: "close 1? (y/n)" }, 900, 7.2).map((s) => s.id)).not.toContain("background");
    expect(ids(400, { ...running, background: 2 })).toContain("background");
    expect(ids(400, { ...running, background: 2 })).not.toContain("model");
  });
});

function roster(...kinds: TaskKind[]): BackgroundTaskRecord[] {
  const tasks: BackgroundTask[] = kinds.map((kind, i) => ({ task_id: `${i}`, run_id: null, kind,
    task_type: kind === "monitor" || kind === "shell" ? "local_bash" : kind,
    description: `task ${i}`, subagent_type: null, parent_task_id: null, tool_use_id: null,
    ambient: false, workflow_name: null }));
  return applyEvent(initialState(), { type: "background_tasks_changed", occurred_at: 1000, tasks }).backgroundTasks.rows;
}

it("uses the CLI's homogeneous task words and its mixed or unknown kind fallback", () => {
  expect(taskWords(roster("shell"))).toBe("1 shell");
  expect(taskWords(roster("shell", "shell", "monitor"))).toBe("2 shells, 1 monitor");
  expect(taskWords(roster("monitor", "monitor"))).toBe("2 monitors");
  expect(taskWords(roster("local_agent"))).toBe("1 local agent");
  expect(taskWords(roster("workflow"))).toBe("1 background dynamic workflow");
  expect(taskWords(roster("shell", "local_agent"))).toBe("2 background tasks");
  const distinctTypes = roster("shell", "monitor");
  distinctTypes[1].task_type = "monitor_mcp";
  expect(taskWords(distinctTypes)).toBe("2 background tasks");
  expect(taskWords(roster("other"))).toBe("1 background task");
  expect(taskWords([])).toBe("");
});

it("falls back to the generic count before dropping a pill that cannot fit", () => {
  const facts = { ...idle, model: null, position: null, backgroundTasks: roster("workflow", "workflow") };
  expect(bandLayout(facts, 900, 7.2).find((s) => s.id === "background")?.text).toBe("2 background dynamic workflows");
  expect(bandLayout(facts, 300, 7.2).find((s) => s.id === "background")?.text).toBe("2 background tasks");
  expect(bandLayout({ ...facts, backgroundTasks: [] }, 900, 7.2).find((s) => s.id === "background")).toBeUndefined();
  expect(backgroundSegment(2)).toBe("2 running in background");
});

it("names the newest live workflow's progress, usage, pause and fallback", () => {
  const tasks = roster("workflow", "shell");
  const workflow = tasks.find((r) => r.kind === "workflow")!;
  workflow.workflow_name = "Research\u202e";
  workflow.usage = { total_tokens: 210000, duration_ms: 252000, tool_uses: 0 };
  workflow.workflow_progress = { phases: [], agents: Array.from({ length: 8 }, (_, index) => ({ index, label: "a", phase_index: 0,
    state: index < 3 ? "done" : "running", model: null, agent_type: null, tokens: null, tool_calls: null, duration_ms: null, error: null })) };
  expect(tasksLine(tasks, 253000)).toBe("⏵ Research⟨U+202E⟩ · 3/8 agents · 4m 12s · 210k tokens · /tasks · +1 more");
  workflow.status = "paused";
  expect(tasksLine(tasks, 253000)).toContain(" · paused · ");
  workflow.status = "running";
  workflow.workflow_progress = null;
  workflow.usage = null;
  expect(tasksLine([workflow], 253000)).toBe("⏵ Research⟨U+202E⟩ · running · 4m 12s · /tasks");
  workflow.ambient = true;
  expect(liveTasks(tasks)).toHaveLength(1);
  tasks[1].live = false;
  expect(liveTasks(tasks)).toHaveLength(0);
});


describe("a segment dropped for room does not come straight back", () => {
  const facts: BandFacts = { ...running, unread: null, usage: { text: "12.3k tok $0.10", title: "t" } };
  // The narrowest band, in whole characters, that still holds the usage figure.
  const fits = (() => {
    for (let cells = 20; cells < 200; cells++) if (ids(cells * 7.2, facts).includes("usage")) return cells;
    throw new Error("never fits");
  })();
  const at = (cells: number) => cells * 7.2;

  it("a width that wobbles across the edge drops the figure once and keeps it dropped", () => {
    let hold: LayoutHold | null = null;
    const seen: boolean[] = [];
    for (let i = 0; i < 20; i++) {
      const r = bandLayoutSteady(facts, at(i % 2 === 0 ? fits - 1 : fits), 7.2, { hold, now: 10_000 + i * 100 });
      hold = r.hold;
      seen.push(r.segs.some((s) => s.id === "usage"));
    }
    expect(seen[0]).toBe(false);
    expect(seen.some((s) => s)).toBe(false);
  });

  it("it comes back once there is room plus a margin, and not within a second of going", () => {
    let r = bandLayoutSteady(facts, at(fits - 1), 7.2, { hold: null, now: 10_000 });
    expect(r.segs.some((s) => s.id === "usage")).toBe(false);
    r = bandLayoutSteady(facts, at(fits + RETURN_MARGIN), 7.2, { hold: r.hold, now: 10_500 });
    expect(r.segs.some((s) => s.id === "usage")).toBe(false);
    expect(r.due).toBe(10_000 + RETURN_COOLDOWN_MS);
    r = bandLayoutSteady(facts, at(fits + RETURN_MARGIN - 1), 7.2, { hold: r.hold, now: 11_200 });
    expect(r.segs.some((s) => s.id === "usage")).toBe(false);
    r = bandLayoutSteady(facts, at(fits + RETURN_MARGIN), 7.2, { hold: r.hold, now: 11_300 });
    expect(r.segs.some((s) => s.id === "usage")).toBe(true);
  });

  it("a segment that no longer fits goes at once, hold or not", () => {
    const wide = bandLayoutSteady(facts, at(fits + 10), 7.2, { hold: null, now: 10_000 });
    expect(wide.segs.some((s) => s.id === "usage")).toBe(true);
    const narrow = bandLayoutSteady(facts, at(fits - 1), 7.2, { hold: wide.hold, now: 10_050 });
    expect(narrow.segs.some((s) => s.id === "usage")).toBe(false);
  });

  it("a reserved width counts toward what fits, and is carried on the segment", () => {
    const r = bandLayoutSteady({ ...facts, position: "9/120" }, at(200), 7.2, { hold: null, now: 0, reserve: { position: 7 } });
    expect(r.segs.find((s) => s.id === "position")?.cells).toBe(7);
    const plain = bandLayoutSteady({ ...facts, position: "9/120" }, at(fits), 7.2, { hold: null, now: 0 });
    const reserved = bandLayoutSteady({ ...facts, position: "9/120" }, at(fits), 7.2, { hold: null, now: 0, reserve: { position: 40 } });
    expect(plain.segs.length).toBeGreaterThan(reserved.segs.length);
  });

  it("only what was dropped is held back: an indicator that appears later is laid out on its own merits", () => {
    // A long flash pushes the band through every step, the card step included, with no card on the band.
    const narrow = 25 * 7.2;
    const quiet: BandFacts = { ...idle, unread: null, position: null, model: null, usage: null, context: null, pill: "⏵⏵ manual" };
    const flash = bandLayoutSteady({ ...quiet, message: "a flash far too long for a band of twenty-five cells" }, narrow, 7.2, { hold: null, now: 10_000 });
    // The flash goes and a card arrives at once: `⚑1` fits, and was never dropped, so it is drawn.
    const card = bandLayoutSteady({ ...quiet, cards: 1 }, narrow, 7.2, { hold: flash.hold, now: 10_100 });
    expect(card.segs.map((x) => x.id)).toContain("cards");
  });

  it("bandLayout without a hold is unchanged: no cells on any segment", () => {
    expect(bandLayout(facts, at(200), 7.2).some((s) => s.cells !== undefined)).toBe(false);
  });
});

it("finds no closing path in a long slash-free flash", () => {
  expect(closingPath("a".repeat(200_000))).toBeNull();
});

it.each([
  ["no such file: a/b  ", 14],
  ["no such file: a/b\n\t", 14],
  ["x /", 2],
  ["x a/b c", null],
  ["\u00a0a/b\u2028", 1],
  ["   ", null],
  ["", null],
])("keeps the closing word and its trailing whitespace: %j", (text, start) => {
  expect(closingPath(text)).toEqual(start === null ? null : {
    text: text.slice(0, start) + revealedPathText(text.slice(start)), start,
  });
});


describe("remote tab location", () => {
  it("band shows the host and account", () => {
    expect(bandLayout({ ...idle, host: "example-host", accountName: "work" }, 3000, 7.2)
      .find(s => s.id === "location")?.text).toBe("example-host · account work");
  });
  it("local tab shows no host", () => {
    expect(bandLayout({ ...idle, host: null, accountName: "work" }, 3000, 7.2)
      .find(s => s.id === "location")?.text).toBe("account work");
    expect(bandLayout(idle, 3000, 7.2).some(s => s.id === "location")).toBe(false);
  });
  it("drops location before other segments", () => {
    const facts = { ...idle, host: "example-host", accountName: "work", usage: { text: "10 tok", title: "usage" } };
    const wide = bandLayout(facts, 3000, 7.2);
    const width = wide.reduce((n, s) => n + s.text.length + 2, 0) * 7.2;
    const narrow = bandLayout(facts, width - 30, 7.2);
    expect(narrow.some(s => s.id === "location")).toBe(false);
    expect(narrow.some(s => s.id === "usage")).toBe(true);
  });
  it("link states render their lines", () => {
    expect(remoteLinkLine("example-host", { kind: "disconnected", sinceUnixMs: 42 }))
      .toBe("disconnected from example-host; the agent may still be working");
    expect(remoteLinkLine("example-host", { kind: "reconnecting", attempt: 3, nextInMs: 50 }))
      .toBe("reconnecting to example-host… (attempt 3)");
    expect(remoteLinkLine("example-host", { kind: "live" })).toBeNull();
    expect(remoteLinkLine(null, null)).toBeNull();
  });
  it("catching up shows done and total", () => {
    expect(remoteLinkLine("example-host", { kind: "catching_up", done: 12, total: 30 }))
      .toBe("catching up 12 / 30");
  });
});
