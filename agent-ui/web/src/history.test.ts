// @vitest-environment node
import { describe, expect, it } from "vitest";
import { historyNoticeText } from "./history";
import type { HistoryNotice } from "./types";

function notice(overrides: Partial<HistoryNotice> = {}): HistoryNotice {
  return {
    source: "claude_transcript",
    restoredItems: 42,
    omittedItems: 0,
    uptoSeq: 43,
    sourcePath: "/home/user/.claude/projects/-home-user-p/7432932d.jsonl",
    attemptedTranscriptPath: null,
    fallbackReason: null,
    writerVersion: "2.1.272",
    ...overrides,
  };
}

describe("historyNoticeText", () => {
  /* The four wordings of design §5.5's table, one example each. */

  it("says where a complete history came from", () => {
    expect(historyNoticeText(notice())).toBe("Earlier messages in this session, read from Claude's own transcript.");
  });

  it("counts what truncation dropped, when it can be counted", () => {
    expect(historyNoticeText(notice({ omittedItems: 1431 }))).toBe(
      "The last 42 records of this session. 1,431 earlier records were not loaded.",
    );
  });

  it("says so plainly when the dropped records cannot be counted", () => {
    expect(historyNoticeText(notice({ omittedItems: null }))).toBe(
      "The last 42 records of this session. Earlier records were not loaded (count unknown).",
    );
  });

  it("names the fallback, its reason, and that the two sources can differ", () => {
    expect(
      historyNoticeText(
        notice({
          source: "eitri_copy",
          fallbackReason: "transcript file not found",
          attemptedTranscriptPath: "/home/user/.claude/projects/-home-user-p/7432932d.jsonl",
          writerVersion: null,
          sourcePath: "/home/user/.local/state/eitri/history/conv/sess.json",
        }),
      ),
    ).toBe(
      "Claude's own transcript was not used (transcript file not found). Showing Eitri's own copy instead — the two can differ.",
    );
  });

  /* A truncated fallback is both of those things at once. §5.5's table lists four cases and this is
     not one of them, but two invariants each demand one of the two sentences -- 14 ("falling back
     to A always says so on screen") and 9 ("truncation is always said on screen") -- so the only
     honest answer is both, in that order. Choosing one would break the other invariant silently. */
  it("says both when the fallback copy was also truncated", () => {
    const text = historyNoticeText(
      notice({ source: "eitri_copy", fallbackReason: "unknown history format version 2", omittedItems: 7 }),
    );
    expect(text).toContain("Showing Eitri's own copy instead");
    expect(text).toContain("7 earlier records were not loaded");
  });

  /* Not "0 earlier records were not loaded": nothing was dropped, so nothing is announced. */
  it("treats zero omitted items as not truncated at all", () => {
    expect(historyNoticeText(notice({ omittedItems: 0 }))).not.toContain("not loaded");
  });

  /* **The count is this restore's own, never a build-wide cap.** This sentence used to print
     `HISTORY_MAX_ITEMS` -- the 400-item ceiling -- on every truncated restore, and the whole-branch
     review measured a real session restoring 32 items that would have read "The last 400 records".
     Two notices differing only in `restoredItems` have to produce two different sentences; a
     constant, in either language, cannot do that. The second assertion is what stops the first
     from being satisfied by a literal that happens to equal this fixture's 42: no multi-digit
     number may appear in the function's own source at all (`> 0` is the only digit that may). */
  it("prints the restore's own count, not a ceiling shared by every restore", () => {
    expect(historyNoticeText(notice({ restoredItems: 32, omittedItems: 64 }))).toContain("The last 32 records");
    expect(historyNoticeText(notice({ restoredItems: 107, omittedItems: 193 }))).toContain("The last 107 records");
    expect(historyNoticeText.toString()).not.toMatch(/\d{2,}/);
  });

  /* A count of exactly one, in both halves of the same sentence. Reachable only since the number
     became this restore's own: with the 400-item ceiling the plural was right by construction, and
     now the character budget can leave a single item (Rust's
     `at_least_one_item_survives_even_when_it_alone_exceeds_the_budget` pins that it does). The
     number stays in the singular wording rather than becoming "The last record of this session",
     so a manual pass still has a count to check the sentence against. */
  it("says record, not records, when exactly one was kept or dropped", () => {
    expect(historyNoticeText(notice({ restoredItems: 1, omittedItems: 1 }))).toBe(
      "The last 1 record of this session. 1 earlier record was not loaded.",
    );
    expect(historyNoticeText(notice({ restoredItems: 1, omittedItems: 2 }))).toBe(
      "The last 1 record of this session. 2 earlier records were not loaded.",
    );
    expect(historyNoticeText(notice({ restoredItems: 2, omittedItems: 1 }))).toBe(
      "The last 2 records of this session. 1 earlier record was not loaded.",
    );
  });

  /* The cap is not on the wire and no longer anywhere on this side either: `HistoryNotice`
     describes ONE restore, not the build's limits, and with the sentence printing the restore's own
     number there is nothing left for a second copy of Rust's constant to be right or wrong about.
     The cross-language tie that used to live here (reading `transcript_jsonl.rs` as text) went with
     it -- a test pinning a constant nothing reads is drift-bait of its own. */
  it("exports no ceiling for anything to drift from", async () => {
    const module = await import("./history");
    expect(Object.keys(module)).toEqual(["historyNoticeText"]);
  });
});
