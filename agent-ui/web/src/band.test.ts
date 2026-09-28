import { describe, expect, it } from "vitest";
import { bandLayout, textWidth, type BandFacts } from "./band";

const running: BandFacts = { mode: "input", pill: "⏵⏵ auto", showcmd: null, message: null, prompt: null, warn: null,
  unread: "↓3", cards: 1, queued: 1, context: { file: "neovibe.zsh", lines: [3, 9] }, position: "14/30", model: "sonnet-5" };
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
    expect(seg?.text === "⧉ neovibe.zsh" || seg === undefined).toBe(true);
    expect(bandLayout(running, 520, 7.2).find((s) => s.id === "context")!.text).toBe("⧉ neovibe.zsh:3-9");
  });
  it("a y/n prompt takes everything right of the mode", () => {
    expect(ids(520, { ...running, prompt: "close 2 \"docs\"? (y/n)" })).toEqual(["mode", "prompt"]);
  });
  it("shows mode and pill before the width is known (Review Focus 3)", () => {
    expect(ids(0, running)).toEqual(["mode", "pill"]);
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
  const R06_PROMPT = "切到 bypass 并批准 1 张等待中的卡片？(y/n)";
  const EMPTY_TAB_PROMPT = "切到 bypass？本窗口之后的新会话也用 bypass (y/n)";
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
