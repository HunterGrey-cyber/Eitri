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
  it("counts omitted ACP turns without describing them as records or a Claude transcript", () => {
    expect(historyNoticeText(notice({ source: "acp_replay", omittedTurns: 400, sourcePath: "" }))).toBe(
      "Earlier messages in this session, replayed by the agent. 400 earlier turns not loaded.",
    );
    expect(historyNoticeText(notice({ source: "acp_replay", omittedTurns: 1 }))).toContain("1 earlier turn not loaded.");
    expect(historyNoticeText(notice({ source: "acp_replay", omittedTurns: 0 }))).not.toContain("not loaded");
  });
  /* The four wordings of the restored-history table, one example each. */

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

  /* A truncated fallback is both of those things at once. The restored-history table lists four cases and this is
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

  /* The count is this restore's own, never a build-wide cap: a 400-item ceiling would mislabel a
     measured 32-item restore. Two notices differing only in `restoredItems` must produce different
     sentences. Reject multi-digit literals in the function source so a hard-coded fixture count
     cannot pass; `> 0` is the only digit that may appear. */
  it("prints the restore's own count, not a ceiling shared by every restore", () => {
    expect(historyNoticeText(notice({ restoredItems: 32, omittedItems: 64 }))).toContain("The last 32 records");
    expect(historyNoticeText(notice({ restoredItems: 107, omittedItems: 193 }))).toContain("The last 107 records");
    expect(historyNoticeText.toString()).not.toMatch(/\d{2,}/);
  });

  /* The character budget can leave exactly one item (Rust's
     `at_least_one_item_survives_even_when_it_alone_exceeds_the_budget`). Keep its count even in the
     singular so a manual pass can check it against the restored list. */
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

  /* `HistoryNotice` describes one restore, not the build's limits. A second copy of Rust's cap
     would be unused and could drift, so this module exports no ceiling. */
  it("exports no ceiling for anything to drift from", async () => {
    const module = await import("./history");
    expect(Object.keys(module)).toEqual(["earlierTurnsText", "historyNoticeText"]);
  });
});
