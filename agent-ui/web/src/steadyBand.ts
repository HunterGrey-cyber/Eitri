import { textWidth, type BandFacts, type SegId, type UsageFact } from "./band";

/** The band is a statusline: still while you read. While a turn streams, its figures (the position, the usage, the
 *  `↓N` count) are redrawn at most once in this many milliseconds, all on the same beat. The state behind them
 *  changes as often as items arrive; only what is drawn waits, and the newest values land at the end of the wait. */
export const FIGURE_INTERVAL_MS = 1000;
/** `↓N` is drawn only once the view has had unseen rows (or been away from the end) this long: a moment of not
 *  following, which the list can pass through while it snaps to new content, never draws it. */
export const UNREAD_SHOW_MS = 500;
/** A position change this soon after a key (outside INSERT, where keys go to the composer) is the reader's own
 *  move, drawn at once. Any other change while a turn streams -- an item arriving, the cursor following the newest
 *  row, rows folding into a run, all of which land in renders of their own -- waits its turn. */
export const OWN_MOVE_MS = 300;
/** The cells `↓N` keeps while a turn runs, whatever its count: the arrow and two digits. */
const UNREAD_MIN_CELLS = 3;

/** One figure as last drawn. */
type Drawn<T> = { value: T | null };

/** What `steadyFacts` carries from one render to the next. */
export type SteadyMemo = {
  position: Drawn<string>;
  usage: Drawn<UsageFact>;
  unread: Drawn<string>;
  /** When a figure was last redrawn: the beat every figure shares. */
  drawnAt: number;
  /** Since when the unseen state has held without a break, while `↓N` is not yet drawn; `null` when it has not. */
  unreadSince: number | null;
  /** The widest each figure has been drawn during this turn, in cells; empty between turns. */
  widest: Partial<Record<SegId, number>>;
  /** Whose figures these are (`BandFacts.scope`): on another tab nothing drawn here is carried over. */
  scope: BandFacts["scope"];
};

export function initialSteady(): SteadyMemo {
  return {
    position: { value: null },
    usage: { value: null },
    unread: { value: null },
    drawnAt: -Infinity,
    unreadSince: null,
    widest: {},
    scope: undefined,
  };
}

const total = (position: string | null) => position?.split("/")[1] ?? null;

/** Draws `raw` now when that is allowed (`free`, or the beat has come), else keeps what is drawn and says when it
 *  may change. `moved` is whether the drawn value changed. */
function throttle<T>(drawn: Drawn<T>, raw: T | null, same: (a: T | null, b: T | null) => boolean, beat: number, now: number, free: boolean) {
  if (same(drawn.value, raw)) return { drawn, due: null, moved: false };
  if (free || now - beat >= FIGURE_INTERVAL_MS) return { drawn: { value: raw }, due: null, moved: true };
  return { drawn, due: beat + FIGURE_INTERVAL_MS, moved: false };
}

/** The facts the band draws this render, given the facts as they are: figures throttled while a turn streams,
 *  `↓N` held back until it has had a reason to be there for `UNREAD_SHOW_MS`, and the width each figure keeps
 *  (`reserve`, in cells) so a count gaining a digit does not push its neighbours. `due` is when something held back
 *  should be drawn even if nothing renders the band again; `null` when nothing waits. Pure: the caller keeps
 *  `memo` for the next render. */
export function steadyFacts(memo: SteadyMemo, facts: BandFacts, now: number): {
  facts: BandFacts;
  memo: SteadyMemo;
  reserve: Partial<Record<SegId, number>>;
  due: number | null;
} {
  const streaming = facts.streaming === true;
  const dues: number[] = [];
  // Another tab's figures: nothing of this one's is held back or kept wide for it.
  if (facts.scope !== memo.scope) memo = { ...initialSteady(), scope: facts.scope };
  // The reader's own move (`j`, `k`, `G`, a search): the band is drawn at once, every figure as it is now, so keys
  // pressed in a row never keep the other figures waiting behind the position.
  const ownMove = facts.mode !== "input" && facts.keyAge !== undefined && facts.keyAge < OWN_MOVE_MS;
  const beat = memo.drawnAt;
  const position = throttle(memo.position, facts.position, (a, b) => a === b, beat, now, !streaming || ownMove);
  const usage = throttle(memo.usage, facts.usage, (a, b) => a?.text === b?.text && a?.title === b?.title, beat, now, !streaming || ownMove);
  let moved = position.moved || usage.moved;
  for (const r of [position, usage]) if (r.due !== null) dues.push(r.due);

  let unread = memo.unread;
  let unreadSince = memo.unreadSince;
  if (facts.unread === null) {
    // The reader reached the end (or jumped there): gone at once.
    unread = { value: null };
    unreadSince = null;
  } else if (unread.value === null) {
    unreadSince = unreadSince ?? now;
    if (now - unreadSince >= UNREAD_SHOW_MS) {
      unread = { value: facts.unread };
      moved = true;
    } else dues.push(unreadSince + UNREAD_SHOW_MS);
  } else {
    const r = throttle(unread, facts.unread, (a, b) => a === b, beat, now, !streaming || ownMove);
    unread = r.drawn;
    moved ||= r.moved;
    if (r.due !== null) dues.push(r.due);
  }

  const drawnFacts: BandFacts = { ...facts, position: position.drawn.value, usage: usage.drawn.value, unread: unread.value };

  // The position always keeps the width of `M/M`: `N` never exceeds `M`, so moving the cursor moves nothing.
  // While a turn runs every figure also keeps the widest it has been this turn; at the turn's end they let go.
  const reserve: Partial<Record<SegId, number>> = {};
  const widest: Partial<Record<SegId, number>> = streaming ? { ...memo.widest } : {};
  const m = total(drawnFacts.position);
  const own: [SegId, number | null][] = [
    ["position", drawnFacts.position === null ? null : textWidth(m === null ? drawnFacts.position : `${m}/${m}`)],
    ["usage", drawnFacts.usage === null ? null : textWidth(drawnFacts.usage.text)],
    ["unread", drawnFacts.unread === null ? null : textWidth(drawnFacts.unread)],
  ];
  for (const [id, cells] of own) {
    if (cells === null) continue;
    // `↓N` keeps room for two digits from the start (`↓ ⚑` is as wide): a count passing 9 moves nothing.
    const floor = id === "unread" ? UNREAD_MIN_CELLS : 0;
    const wide = streaming ? Math.max(widest[id] ?? 0, cells, floor) : cells;
    if (streaming) widest[id] = wide;
    if (id === "position" || streaming) reserve[id] = wide;
  }

  return {
    facts: drawnFacts,
    memo: { position: position.drawn, usage: usage.drawn, unread, drawnAt: moved ? now : beat, unreadSince, widest, scope: facts.scope },
    reserve,
    due: dues.length === 0 ? null : Math.min(...dues),
  };
}
