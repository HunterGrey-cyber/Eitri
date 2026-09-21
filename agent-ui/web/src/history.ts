import type { HistoryNotice } from "./types";

/** English, like every other string in this panel (`Running…`, `Ask the agent...`, `New session`),
 *  and grouped rather than locale-aware: `en-US` is named explicitly so a test does not depend on
 *  the machine's locale and a reader does not depend on it either. */
function count(value: number): string {
  return value.toLocaleString("en-US");
}

/** `1 record` / `42 records`.
 *
 *  While this sentence printed the build-wide 400-item
 *  ceiling the plural was right by construction, and now that the number is this restore's own,
 *  "The last 1 records of this session." is reachable -- the character budget can leave exactly one
 *  item, which `at_least_one_item_survives_even_when_it_alone_exceeds_the_budget` pins in Rust.
 *  The number is kept rather than dropped for the singular ("The last record of this session"),
 *  because a count after "The last" is what a manual pass checks the sentence against. */
function records(value: number): string {
  return `${count(value)} ${value === 1 ? "record" : "records"}`;
}

/**
 * Text for the history notice row. Report the source and truncation independently so a
 * truncated fallback copy gets both sentences.
 *
 * Counts come from this restore because the character budget can bind before the item limit.
 * `restoredItems` counts rows after folding in Rust. Omitted items were dropped before folding
 * and cannot be expressed in rows, so the two counts use different units.
 *
 * Describe what was read, without claiming what the model remembers. The CLI rebuilds its own
 * context; a compaction summary must not be presented as the user's words.
 * A fallback means the transcript was not used, which can happen even when it is readable but
 * contains no restorable items. The backend's reason string explains the cause.
 *
 * Omit the absolute source path here because it would wrap repeatedly in a narrow panel.
 * The row provides a Copy path button instead.
 */
export function historyNoticeText(notice: HistoryNotice): string {
  // `null` means "certainly omitted, not countable" (the scan started partway into a file too large
  // to read whole), so it is truncation just as much as a positive count is.
  const truncated = notice.omittedItems === null || notice.omittedItems > 0;
  const sentences: string[] = [];
  if (notice.source === "neovibe_copy") {
    sentences.push(
      `Claude's own transcript was not used (${notice.fallbackReason ?? "reason unknown"}).` +
        " Showing Neovibe's own copy instead — the two can differ.",
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
