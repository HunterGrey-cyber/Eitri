import type { HistoryNotice } from "./types";

/** English, like every other string in this panel (`Running…`, `Ask the agent...`, `New session`),
 *  and grouped rather than locale-aware: `en-US` is named explicitly so a test does not depend on
 *  the machine's locale and a reader does not depend on it either. */
function count(value: number): string {
  return value.toLocaleString("en-US");
}

/** `1 record` / `42 records`. The character budget can leave exactly one item, as
 *  `at_least_one_item_survives_even_when_it_alone_exceeds_the_budget` pins in Rust. Keep the count
 *  even in the singular so a manual pass can compare the notice to the restored list. */
function records(value: number): string {
  return `${count(value)} ${value === 1 ? "record" : "records"}`;
}

export function earlierTurnsText(turns: number): string {
  return `${count(turns)} earlier ${turns === 1 ? "turn" : "turns"} not loaded.`;
}

/** The notice combines where history came from and whether it was cut. These are independent:
 *  even a truncated fallback needs both sentences, so a four-row table cannot cover every case.
 *  Every number belongs to this restore, never a build-wide ceiling: the character budget can
 *  bind first about as often as the item limit (measured restores include 32 items and 107 of 300).
 *  `restoredItems` counts rows after Rust's fold; omitted counts items dropped before the fold,
 *  which cannot be expressed as rows (`agent/src/history/load.rs::restorable_item_count`). The two
 *  units differ deliberately, and each number is exact in its own unit.
 *  Say only what was read, never what the model remembers: the CLI rebuilds its own context, and
 *  a compaction summary is not the user's words. Say the transcript was not used, not that it
 *  could not be read: a readable file may contain only slash echoes, `isMeta` injections or summaries.
 *  Rust's reason carries the detail. The absolute source path would wrap repeatedly in a narrow
 *  panel; the row has a Copy path button instead. */
export function historyNoticeText(notice: HistoryNotice): string {
  if (notice.source === "acp_replay") {
    const omitted = notice.omittedTurns ?? 0;
    return "Earlier messages in this session, replayed by the agent." +
      (omitted > 0 ? ` ${earlierTurnsText(omitted)}` : "");
  }
  // `null` means "certainly omitted, not countable" (the scan started partway into a file too large
  // to read whole), so it is truncation just as much as a positive count is.
  const truncated = notice.omittedItems === null || notice.omittedItems > 0;
  const sentences: string[] = [];
  if (notice.source === "eitri_copy") {
    sentences.push(
      `Claude's own transcript was not used (${notice.fallbackReason ?? "reason unknown"}).` +
        " Showing Eitri's own copy instead — the two can differ.",
    );
  } else if (!truncated) {
    sentences.push("Earlier messages in this session, read from Claude's own transcript.");
  }
  if (truncated) {
    sentences.push(
      `The last ${records(notice.restoredItems)} of this session.` +
        (notice.omittedItems === null
          ? " Earlier records were not loaded (count unknown)."
          : ` ${count(notice.omittedItems)} earlier ${notice.omittedItems === 1 ? "record was" : "records were"} not loaded.`),
    );
  }
  return sentences.join(" ");
}
