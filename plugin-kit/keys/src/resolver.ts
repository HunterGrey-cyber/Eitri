import { notationOf, parseNotation, type KeySeq, type KeyStroke } from "./notation";
import type { Row } from "./rows";

export type Resolution =
  | { kind: "action"; action: string }
  | { kind: "fallback"; action: string; consumed: number }
  | { kind: "pending"; prefix: string; length: number }
  | { kind: "none" };

const same = (a: KeyStroke, b: KeyStroke) => a.key === b.key && a.ctrl === b.ctrl && a.shift === b.shift;

export function createResolver(rows: Row[], timeoutlen: () => number, now: () => number) {
  const bindings = rows.flatMap((row) => row.keys.flatMap((key) => {
    const seq = parseNotation(key);
    return seq ? [{ seq, action: row.action }] : [];
  }));
  let pending: KeySeq = [];
  // A key that is itself bound and also starts a longer binding waits like any prefix, as vim does; this is the
  // action it fires if nothing longer follows.
  let fallback: { action: string; length: number } | undefined;
  let last = 0;
  const reset = () => { pending = []; fallback = undefined; last = 0; };
  const matching = (keys: KeySeq) => bindings.filter(({ seq }) => seq.length >= keys.length &&
    keys.every((part, i) => same(part, seq[i])));
  // Only the fallback's own strokes are consumed; the caller retains the events for the remaining prefix and ending key.
  const end = (): Resolution => {
    const completed = fallback;
    reset();
    return completed ? { kind: "fallback", action: completed.action, consumed: completed.length } : { kind: "none" };
  };

  return {
    reset,
    /** Resolves the wait and identifies the strokes its fallback consumes; clears the pending keys. */
    expire(): Resolution {
      return end();
    },
    feed(stroke: KeyStroke): Resolution {
      const time = now();
      if (pending.length && time - last > timeoutlen()) return end();
      let keys = [...pending, stroke];
      let matches = matching(keys);
      // Esc that continues no sequence cancels the pending keys, without their action, and then counts alone.
      if (!matches.length && pending.length && stroke.key === "Escape") {
        reset();
        keys = [stroke];
        matches = matching(keys);
      }
      if (!matches.length) return end();
      const exact = matches.find(({ seq }) => seq.length === keys.length);
      if (exact && matches.every(({ seq }) => seq.length === keys.length)) {
        reset();
        return { kind: "action", action: exact.action };
      }
      pending = keys;
      if (exact) fallback = { action: exact.action, length: keys.length };
      last = time;
      return { kind: "pending", prefix: pending.map(notationOf).join(""), length: pending.length };
    },
  };
}
