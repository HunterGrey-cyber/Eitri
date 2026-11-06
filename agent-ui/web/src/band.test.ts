import { describe, expect, it } from "vitest";
import { backgroundSegment, bandLayout, cardSummary, liveTasks, taskWords, tasksLine, textWidth, usageSegment, type BandFacts } from "./band";
import { applyEvent, initialState } from "./reducer";
import type { BackgroundTask, BackgroundTaskRecord, TaskKind, TokenUsage } from "./types";

const running: BandFacts = { mode: "input", pill: "⏵⏵ auto", showcmd: null, message: null, prompt: null, warn: null,
  unread: "↓3", cards: 1, queued: 1, context: { file: "eitri.zsh", lines: [3, 9] }, position: "14/30", model: "sonnet-5", usage: null };
const idle: BandFacts = { ...running, mode: "browse", unread: null, cards: 0, queued: 0, context: null };
const ids = (w: number, f: BandFacts) => bandLayout(f, w, 7.2).map((s) => s.id);

describe("the phone mark", () => {
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
  it("drops the review pointer before the model, and never over a prompt", () => {
    const facts: BandFacts = { ...idle, review: { files: 3 } };
    expect(ids(900, facts)).toContain("review");
    // Narrow enough that the model has to go too: the pointer went first.
    const narrow = bandLayout(facts, 200, 7.2).map((s) => s.id);
    expect(narrow).not.toContain("review");
    for (let width = 100; width < 900; width += 10) {
      const shown = bandLayout(facts, width, 7.2).map((s) => s.id);
      if (shown.includes("review")) expect(shown, `width ${width}`).toContain("model");
    }
    expect(ids(900, { ...facts, prompt: "close? (y/n)" })).toEqual(["mode", "prompt"]);
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
  it("drops the effort after the review pointer and before the model, and never leaves it alone", () => {
    const facts: BandFacts = { ...idle, effort: "xhigh", review: { files: 3 } };
    expect(ids(900, facts)).toContain("effort");
    const widths = Array.from({ length: 80 }, (_, i) => 100 + i * 10);
    const shown = widths.map((width) => bandLayout(facts, width, 7.2).map((s) => s.id));
    for (const [i, row] of shown.entries()) {
      if (row.includes("effort")) expect(row, `width ${widths[i]}`).toContain("model");
      if (row.includes("review")) expect(row, `width ${widths[i]}`).toContain("effort");
    }
    expect(shown.some((row) => row.includes("model") && !row.includes("effort")), "a width keeps the model alone").toBe(true);
    expect(shown.some((row) => row.includes("effort") && !row.includes("review")), "a width keeps the effort").toBe(true);
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
 *  character. A y/n prompt must keep its own `(y/n)`, and a typing-guard flash its explanation. */
describe("a y/n prompt or a flash is never truncated", () => {
  const NARROW_BUDGET_WIDTH = 340; // floor(340 / 7.2) === 47
  const R06_PROMPT = "Switch to bypass and approve the 1 waiting card? (y/n)";
  const EMPTY_TAB_PROMPT = "Switch to bypass? New sessions in this window start in bypass too (y/n)";
  const D11_FLASH = "y must be pressed on its own to enter bypass — Shift+Tab to ask again";

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

  it("keeps the flash that explains y whole -- it explains what y does, not just a status word", () => {
    const segs = bandLayout({ ...idle, message: D11_FLASH }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(D11_FLASH);
    expect(message.text).not.toContain("…");
    expect(message.wraps).toBe(true);
  });

  /* The sentence Rust says when a tab's agent has no manual mode (`TabSet::mode_offered_to`): it arrives as a
     notice, which the band shows as its flash. At the panel's 480px it is about 100 columns of text. */
  it("keeps the 'has no manual mode' sentence whole beside a manual pill, wrapping at 480px", () => {
    const note =
      "gemini tabs have no manual mode (the agent's own mode is set once, at its start): tab 2 is in auto";
    const segs = bandLayout({ ...idle, pill: "⏵⏵ manual", message: note }, 480, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(note);
    expect(message.wraps).toBe(true);
    expect(segs.find((s) => s.id === "pill")!.text).toBe("⏵⏵ manual");
    expect(segs.find((s) => s.id === "mode")!.text).toBe("BROWSE");
  });

  it("a flash that wraps leaves the mode its word: the letter would not make it fit", () => {
    const segs = bandLayout({ ...idle, message: D11_FLASH }, NARROW_BUDGET_WIDTH, 7.2);
    expect(segs.find((s) => s.id === "mode")!.text).toBe("BROWSE");
    expect(segs.find((s) => s.id === "message")!.wraps).toBe(true);
    // ...while a band with no message still shortens the mode when nothing else is left to drop.
    expect(bandLayout({ ...idle, warn: "skew" }, 120, 7.2).find((s) => s.id === "mode")!.text).toBe("B");
  });

  it("the letter still replaces the word when that is what lets a message fit", () => {
    // mode 6 + pill 7 + message 28 + 3 * 2 padding = 47 > 44; with the letter, 42 fits.
    const message = "x".repeat(28);
    const segs = bandLayout({ ...idle, message }, 44 * 7.2, 7.2);
    expect(segs.find((s) => s.id === "mode")!.text).toBe("B");
    expect(segs.find((s) => s.id === "message")!.wraps).toBeFalsy();
  });

  it("a short prompt still fits the row whole -- no wraps flag", () => {
    const shortPrompt = 'close 1 "docs"? (y/n)';
    const segs = bandLayout({ ...idle, prompt: shortPrompt }, NARROW_BUDGET_WIDTH, 7.2);
    const prompt = segs.find((s) => s.id === "prompt")!;
    expect(prompt.text).toBe(shortPrompt);
    expect(prompt.wraps).toBeFalsy();
  });

  it("a short flash still fits the row whole -- no wraps flag", () => {
    const segs = bandLayout({ ...idle, message: "copied 12 chars" }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe("copied 12 chars");
    expect(message.wraps).toBeFalsy();
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
  it("sits right of the model and is the first thing dropped", () => {
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

/** While a card waits and the composer has the keys, the band names
 *  the card INPUT's Ctrl+y would approve -- the tool and a short summary -- and drops it by the same
 *  priority order as everything else: shortened to the tool, then gone, after the context, model,
 *  position and queue and before the `⚑N` count. */
describe("the Ctrl+y segment", () => {
  const approving: BandFacts = { ...running, approve: { tool: "Bash", summary: "rm build" } };

  it("names the tool and the summary, right after the card count, on the left", () => {
    const segs = bandLayout(approving, 1400, 7.2);
    expect(segs.map((s) => s.id).slice(0, 4)).toEqual(["mode", "pill", "cards", "approve"]);
    const seg = segs.find((s) => s.id === "approve")!;
    expect(seg.text).toBe("Ctrl+y approves Bash: rm build");
    expect(seg.side).toBe("left");
  });

  it("is absent with nothing to approve, and under a y/n prompt", () => {
    expect(ids(1400, running)).not.toContain("approve");
    expect(ids(1400, { ...approving, approve: null })).not.toContain("approve");
    expect(ids(1400, { ...approving, prompt: "close 2? (y/n)" })).toEqual(["mode", "prompt"]);
  });

  it("goes after context, model, position and queue, is shortened to the tool first, and goes before the card count", () => {
    let sawShort = false;
    for (let w = 1400; w >= 60; w -= 5) {
      const segs = bandLayout(approving, w, 7.2);
      const idsNow = segs.map((s) => s.id);
      const seg = segs.find((s) => s.id === "approve");
      if (seg === undefined || seg.text !== "Ctrl+y approves Bash: rm build") {
        for (const gone of ["usage", "model", "position", "context", "queue"]) expect(idsNow, `width ${w}`).not.toContain(gone);
      }
      if (seg?.text === "Ctrl+y approves Bash") sawShort = true;
      if (!idsNow.includes("cards")) expect(idsNow, `width ${w}`).not.toContain("approve");
    }
    expect(sawShort).toBe(true);
  });

  it("summarizes a card in one short line: the command or path first, whitespace collapsed, long input cut", () => {
    expect(cardSummary({ command: "rm   build\n&& ls", description: "Remove the build dir" })).toBe("rm build && ls");
    expect(cardSummary({ file_path: "/p/src/a.ts", content: "x" })).toBe("/p/src/a.ts");
    expect(cardSummary({ url: "https://example.com/x" })).toBe("https://example.com/x");
    const long = cardSummary({ command: "x".repeat(200) });
    expect(long.length).toBe(40);
    expect(long).toContain("…");
    expect(cardSummary({ other: 1 })).toBe('{"other":1}');
    expect(cardSummary(null)).toBe("null");
  });
});

describe("cardSummary on an ACP agent's card (`{title, rawInput, locations, diffs}`)", () => {
  it("names what the call acts on from the agent's raw input, as for a Claude card", () => {
    expect(cardSummary({ title: "Run `touch card-marker`", rawInput: { command: ["touch", "card-marker"] }, locations: [], diffs: [] })).toBe(
      "touch card-marker",
    );
    expect(cardSummary({ title: "Run a script", rawInput: { command: "make   test" }, locations: [], diffs: [] })).toBe("make test");
    expect(cardSummary({ title: "Edit a.ts", rawInput: { path: "/p/a.ts", newText: "x" }, locations: [{ path: "/p/a.ts" }], diffs: [] })).toBe(
      "/p/a.ts",
    );
  });

  it("falls back to the raw input as JSON, then to the first location, then to the agent's own title, never to the whole card as JSON", () => {
    // The title and the locations are the agent's words about the call; a raw input with no key the band
    // knows is still the call.
    expect(cardSummary({ title: "Read it", rawInput: { odd: 1 }, locations: [{ path: "/p/b.md" }], diffs: [] })).toBe('{"odd":1}');
    expect(cardSummary({ title: "Read notes", rawInput: { argv: ["rm", "-rf", "x"] }, locations: [], diffs: [] })).toBe('{"argv":["rm","-rf","x"]}');
    expect(cardSummary({ title: "Read it", rawInput: {}, locations: [{ path: "/p/b.md" }], diffs: [] })).toBe("/p/b.md");
    expect(cardSummary({ title: "Read it", rawInput: null, locations: [{ path: "/p/b.md" }], diffs: [] })).toBe("/p/b.md");
    expect(cardSummary({ title: "Fetch the page", rawInput: null, locations: [], diffs: [] })).toBe("Fetch the page");
    expect(cardSummary({ title: "Fetch the page", rawInput: {}, locations: [], diffs: [] })).toBe("Fetch the page");
  });

  it("never lets the agent's prose in the raw input, or a location, stand in front of the key that does the work", () => {
    // `description`, `prompt` and `name` name a Claude tool's call; in an ACP raw input they are the
    // agent's account of it, shown only inside the JSON.
    expect(cardSummary({ title: "t", rawInput: { description: "x", script: "rm -rf ~" }, locations: [], diffs: [] })).toBe(
      '{"description":"x","script":"rm -rf ~"}',
    );
    // Too long for the band, the JSON is cut in the middle and its tail, the script, still shows.
    expect(cardSummary({ title: "t", rawInput: { description: "Read notes", script: "rm -rf ~" }, locations: [], diffs: [] })).toBe(
      '{"description":"…s","script":"rm -rf ~"}',
    );
    expect(cardSummary({ title: "t", rawInput: { prompt: "look", name: "notes" }, locations: [], diffs: [] })).toBe(
      '{"prompt":"look","name":"notes"}',
    );
    expect(cardSummary({ title: "t", rawInput: { script: "rm -rf ~" }, locations: [{ path: "/p/notes.md" }], diffs: [] })).toBe(
      '{"script":"rm -rf ~"}',
    );
    // A key that names the call still wins over the prose next to it.
    expect(cardSummary({ title: "t", rawInput: { description: "Read notes", command: "rm -rf ~" }, locations: [], diffs: [] })).toBe("rm -rf ~");
    // A Claude tool's own `description` (a `Task`) still names its call.
    expect(cardSummary({ description: "Find the bug", prompt: "look at a.ts" })).toBe("Find the bug");
  });

  it("quotes an argument-list word that holds a space, so the words read as the call has them", () => {
    expect(cardSummary({ title: "t", rawInput: { command: ["rm", "my notes.txt"] }, locations: [], diffs: [] })).toBe("rm 'my notes.txt'");
    expect(cardSummary({ title: "t", rawInput: { command: ["bash", "-lc", "ls -la"] }, locations: [], diffs: [] })).toBe("bash -lc 'ls -la'");
    expect(cardSummary({ title: "t", rawInput: { command: ["echo", "it's", ""] }, locations: [], diffs: [] })).toBe("echo 'it'\\''s' ''");
    // A no-break space is whitespace to a reader: the word is quoted, and the space drawn as its escape.
    expect(cardSummary({ title: "t", rawInput: { command: ["rm", "a b"] }, locations: [], diffs: [] })).toBe("rm 'a⟨U+00A0⟩b'");
  });
});

/** A long command is cut in the middle, not at the end, so its tail -- often
 *  the dangerous part (`| sudo bash`, `--force`) -- still shows on the band. */
describe("cardSummary keeps the tail", () => {
  it("cuts a long command in the middle, keeping its head and its end", () => {
    const s = cardSummary({ command: "curl -fsSL https://example.com/some/long/path/install.sh | sudo bash -s -- --force" });
    expect(Array.from(s).length).toBe(40);
    expect(s.startsWith("curl -fsSL")).toBe(true);
    expect(s.endsWith("sudo bash -s -- --force")).toBe(true);
    expect(s).toContain("…");
  });
});

/** The band's `Ctrl+y` segment names the call that key approves, often while the card itself is out of view,
 *  so it must show what the card shows: no character that reorders, hides or fakes text gets through raw. */
describe("cardSummary shows the card's hidden characters as the card does", () => {
  it("draws a direction override, a zero-width space and a no-break space as escapes", () => {
    // U+202E reverses what follows it: drawn raw, `echo ok \u202Efr- mr` reads as `echo ok rm -rf`.
    expect(cardSummary({ command: "echo ok \u202Efr- mr" })).toBe("echo ok ⟨U+202E⟩fr- mr");
    expect(cardSummary({ command: "r\u200Bm x" })).toBe("r⟨U+200B⟩m x");
    // A no-break space is not one the shell splits at, so it is not collapsed into a plain one.
    expect(cardSummary({ command: "rm\u00A0-rf x" })).toBe("rm⟨U+00A0⟩-rf x");
    // Plain spaces, tabs and line breaks still collapse to one space.
    expect(cardSummary({ command: "a \t\n b" })).toBe("a b");
  });
  it("never cuts an escape in half, and keeps the cut within the same length", () => {
    const s = cardSummary({ command: `${"a".repeat(14)}\u202E${"b".repeat(60)}\u202Ezz` });
    expect(Array.from(s).length).toBeLessThanOrEqual(40);
    expect(s).not.toMatch(/⟨U\+[0-9A-F]*…|…[0-9A-F]*⟩/);
    expect(s.endsWith("⟨U+202E⟩zz")).toBe(true);
  });
  it("draws a direction override as its escape on the band, from an ACP card and from a Claude Bash card", () => {
    // Drawn raw, `notes‮fdp.exe` reads as `notesexe.pdf`.
    const acp = cardSummary({ title: "Read notes", rawInput: { command: ["cat", "notes‮fdp.exe"] }, locations: [], diffs: [] });
    expect(acp).toBe("cat notes⟨U+202E⟩fdp.exe");
    const bash = cardSummary({ command: "cat notes‮fdp.exe", description: "Read notes" });
    expect(bash).toBe("cat notes⟨U+202E⟩fdp.exe");
    for (const [tool, summary] of [
      ["shell", acp],
      ["Bash", bash],
    ]) {
      const seg = bandLayout({ ...idle, cards: 1, approve: { tool, summary } }, 2000, 7.2).find((s) => s.id === "approve");
      expect(seg?.text).toBe(`Ctrl+y approves ${tool}: cat notes⟨U+202E⟩fdp.exe`);
      expect(seg?.text).not.toContain("‮");
    }
  });
  it("draws the tool name's hidden characters as escapes too", () => {
    const seg = bandLayout({ ...idle, approve: { tool: "mcp__x__\u202Eevil", summary: "ok" } }, 2000, 7.2).find((s) => s.id === "approve");
    expect(seg?.text).toBe("Ctrl+y approves mcp__x__⟨U+202E⟩evil: ok");
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

it.each([
  ["\u00A0rm file", "⟨U+00A0⟩rm file"],
  ["rm file\r", "rm file⟨U+000D⟩"],
  ["\uFEFFrm file\u2028", "⟨U+FEFF⟩rm file⟨U+2028⟩"],
  [" \t\n rm file \n\t ", "rm file"],
])("preserves escaped whitespace at the edge of a command %j", (command, summary) => {
  expect(cardSummary({ command })).toBe(summary);
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
