import { describe, expect, it } from "vitest";
import { bandLayout, cardSummary, textWidth, usageSegment, type BandFacts } from "./band";
import type { TokenUsage } from "./types";

const running: BandFacts = { mode: "input", pill: "⏵⏵ auto", showcmd: null, message: null, prompt: null, warn: null,
  unread: "↓3", cards: 1, queued: 1, context: { file: "eitri.zsh", lines: [3, 9] }, position: "14/30", model: "sonnet-5", usage: null };
const idle: BandFacts = { ...running, mode: "browse", unread: null, cards: 0, queued: 0, context: null };
const ids = (w: number, f: BandFacts) => bandLayout(f, w, 7.2).map((s) => s.id);

describe("the band degrades by priority (spec §5.3)", () => {
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
  it("a y/n prompt takes everything right of the mode", () => {
    expect(ids(520, { ...running, prompt: "close 2 \"docs\"? (y/n)" })).toEqual(["mode", "prompt"]);
  });
  it("shows mode and pill before the width is known (Review Focus 3)", () => {
    expect(ids(0, running)).toEqual(["mode", "pill"]);
  });
  it("names CARET's own mode word and narrow letter (D16, added for 3a)", () => {
    const caret: BandFacts = { ...idle, mode: "caret" };
    expect(bandLayout(caret, 520, 7.2).find((s) => s.id === "mode")!.text).toBe("CARET");
    const tiny = bandLayout({ ...caret, warn: "skew" }, 120, 7.2);
    expect(tiny.find((s) => s.id === "mode")!.text).toBe("C");
  });
  it("counts East Asian wide characters twice (Review Focus 3)", () => {
    expect(textWidth("zsh 补全")).toBe(8);
    // 8 + 9 + 31 (the wide name) + 10 + 7 = 65 > 50: model and position go, the context fits after.
    expect(ids(360, { ...idle, context: { file: "说明文档非常长的文件名称.md", lines: null } })).toEqual(["mode", "pill", "context"]);
  });
});

/** Defect 1 (2026-09-27 sandbox GUI pass): the owner's own WebKit zoom 1.5 measured the panel at
 *  520 logical px = 346 CSS px, which comes out to a 47-column budget here (47 * 7.2 = 338.4, the
 *  same char width every other test in this file already uses) -- the exact case that used to lose
 *  a y/n prompt's own `(y/n)`, and the D11 flash's own explanation, to `bandLayout`'s old `cut()`. */
describe("defect 1: a y/n prompt or a flash is never truncated", () => {
  const NARROW_BUDGET_WIDTH = 340; // floor(340 / 7.2) === 47
  const R06_PROMPT = "Switch to bypass and approve the 1 waiting card? (y/n)";
  const EMPTY_TAB_PROMPT = "Switch to bypass? New sessions in this window start in bypass too (y/n)";
  const D11_FLASH = "y must be pressed on its own to enter bypass — Shift+Tab to ask again";

  it("keeps the R06 bypass prompt whole, (y/n) included, at a 47-column budget", () => {
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

  it("keeps the D11 flash whole -- it explains what y does, not just a status word", () => {
    const segs = bandLayout({ ...idle, message: D11_FLASH }, NARROW_BUDGET_WIDTH, 7.2);
    const message = segs.find((s) => s.id === "message")!;
    expect(message.text).toBe(D11_FLASH);
    expect(message.text).not.toContain("…");
    expect(message.wraps).toBe(true);
  });

  it("a short prompt still fits the row whole -- no wraps flag, same as before this fix", () => {
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

/** R5 (v1 picks, Task 13): the active tab's last reported usage, right of the model, dropped before
 *  anything else when the band is narrow. */
describe("usage (R5)", () => {
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
  it("a cost that is not a finite number is no measurement: nothing is drawn, and nothing throws", () => {
    expect(usageSegment(u(Number.NaN, { input: 1, output: 1, cache_creation: 0, cache_read: 0 }))).toBeNull();
    expect(usageSegment(u(Number.POSITIVE_INFINITY, null))).toBeNull();
    // What a `null` cost would arrive as if a non-finite float ever crossed the wire as JSON.
    expect(usageSegment(u(null as unknown as number, null))).toBeNull();
  });
});

/** Owner decision #39 (2026-09-30): while a card waits and the composer has the keys, the band names
 *  the card INPUT's Ctrl+y would approve -- the tool and a short summary -- and drops it by the same
 *  priority order as everything else: shortened to the tool, then gone, after the context, model,
 *  position and queue and before the `⚑N` count. */
describe("#39: the Ctrl+y segment", () => {
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

/** Fix round 1 (Opus M-3): a long command is cut in the middle, not at the end, so its tail -- often
 *  the dangerous part (`| sudo bash`, `--force`) -- still shows on the band. */
describe("#39 fix round 1: cardSummary keeps the tail", () => {
  it("cuts a long command in the middle, keeping its head and its end", () => {
    const s = cardSummary({ command: "curl -fsSL https://example.com/some/long/path/install.sh | sudo bash -s -- --force" });
    expect(Array.from(s).length).toBe(40);
    expect(s.startsWith("curl -fsSL")).toBe(true);
    expect(s.endsWith("sudo bash -s -- --force")).toBe(true);
    expect(s).toContain("…");
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
    const narrow = ids(240, facts);
    expect(narrow).toContain("ending");
    expect(narrow).not.toContain("model");
    expect(narrow).not.toContain("cards");
    expect(ids(900, { ...facts, prompt: "close? (y/n)" })).toEqual(["mode", "prompt"]);
  });
});
