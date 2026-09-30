import type { HistoryNotice } from "./types";

/** English, like every other string in this panel (`Running…`, `Ask the agent...`, `New session`),
 *  and grouped rather than locale-aware: `en-US` is named explicitly so a test does not depend on
 *  the machine's locale and a reader does not depend on it either. */
function count(value: number): string {
  return value.toLocaleString("en-US");
}

/** `1 record` / `42 records`.
 *
 *  Newly load-bearing (re-review, 2026-09-20): while this sentence printed the build-wide 400-item
 *  ceiling the plural was right by construction, and now that the number is this restore's own,
 *  "The last 1 records of this session." is reachable -- the character budget can leave exactly one
 *  item, which `at_least_one_item_survives_even_when_it_alone_exceeds_the_budget` pins in Rust.
 *  The number is kept rather than dropped for the singular ("The last record of this session"),
 *  because a count after "The last" is what a manual pass checks the sentence against. */
function records(value: number): string {
  return `${count(value)} ${value === 1 ? "record" : "records"}`;
}

/**
 * The one line the notice row says (design §5.5).
 *
 * Built from two independent statements rather than picked from a table of four, because the two
 * things it can report are independent: WHERE the history came from, and WHETHER it was cut. The
 * design's table lists the four combinations that occur in practice and this produces each of them
 * word for word; the fifth -- a fallback copy that was also truncated -- gets both sentences,
 * because invariant 14 requires the fallback always to be announced and invariant 9 requires
 * truncation always to be announced, and dropping either to fit a four-row table would break one of
 * them silently.
 *
 * **Every number here comes out of this restore, never out of a build-wide constant** (whole-branch
 * review, 2026-09-20). The sentence used to print `HISTORY_MAX_ITEMS`, the 400-item ceiling, as
 * though truncation always stopped there -- but the character budget binds first about as often,
 * and the reviewer measured one real session restoring 32 items while the panel would have said
 * 400, and another restoring 107 of 300. `restoredItems` is now counted in Rust AFTER the fold, so
 * it is the number of rows this list actually holds; the constant that used to live in this file,
 * and the test that tied it to Rust's own source, went with the sentence that needed it.
 *
 * **The two numbers are in different units, deliberately.** Restored is counted in rows; omitted is
 * counted in the items truncation dropped, which no fold ever saw and which therefore cannot be
 * expressed in rows. `agent/src/history/load.rs::restorable_item_count` carries the argument. Both
 * are exact in their own unit, which is the honest pair available.
 *
 * **Says only what was read, never what the agent remembers** (§5.5's wording discipline): what a
 * resumed session actually carries into the model is the CLI's business, rebuilt from its own
 * context, and this side cannot know it. The concrete cautionary case is a compaction summary --
 * text telling the model what it remembers -- which an earlier draft of the parser would have
 * shown as the user's own words.
 *
 * **And says only what was observed about the transcript.** The fallback sentence used to open
 * "Claude's transcript could not be read", which is false for the most ordinary reason of all: a
 * file that reads perfectly and holds nothing this build can restore (a session whose user lines
 * are all slash-command echoes, `isMeta` injections or a compaction summary). It now says the
 * transcript was not USED, which is true whatever the reason, and Rust's own reason string carries
 * the rest.
 *
 * The source path is deliberately not in here: it is an absolute path, it would wrap this line
 * several times in a narrow panel, and the row carries a Copy path button for it instead.
 */
export function historyNoticeText(notice: HistoryNotice): string {
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
