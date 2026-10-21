import { notationOf, parseNotation, type KeySeq, type KeyStroke } from "./notation";
import type { Row } from "./rows";

export type Resolution =
  | { kind: "action"; action: string }
  | { kind: "fallback"; action: string; consumed: number }
  | { kind: "pending"; prefix: string; length: number }
  | { kind: "none" }
  | { kind: "cancelled" };

const same = (a: KeyStroke, b: KeyStroke) => a.key === b.key && a.ctrl === b.ctrl && a.shift === b.shift;

export type ResolverOptions = {
  /** Whether a prefix with no shorter binding waits for its next key with no timeout (the default), as vim's own
   *  `g`, `z`, `[` and `]` do; when false every pending key times out after `timeoutlen`. */
  prefixWait?: () => boolean;
};

export function createResolver(rows: Row[], timeoutlen: () => number, now: () => number, opts?: ResolverOptions) {
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
  const extending = () => (pending.length ? matching(pending).filter(({ seq }) => seq.length > pending.length) : []);
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
    /** True while a shorter completed binding would fire if nothing longer follows. */
    hasFallback(): boolean {
      return fallback !== undefined;
    },
    /** What may follow the pending keys, in declared order, each remainder once. */
    continuations(): { rest: string; action: string }[] {
      const seen = new Set<string>();
      return extending().flatMap(({ seq, action }) => {
        const rest = seq.slice(pending.length).map(notationOf).join("");
        if (seen.has(rest)) return [];
        seen.add(rest);
        return [{ rest, action }];
      });
    },
    feed(stroke: KeyStroke): Resolution {
      const time = now();
      // A wait that nothing shorter can end would otherwise lose the prefix to a pause; with a shorter binding, the
      // timeout is what lets that binding fire on its own.
      const timed = fallback !== undefined || !(opts?.prefixWait?.() ?? true);
      if (pending.length && timed && time - last > timeoutlen()) return end();
      const keys = [...pending, stroke];
      const matches = matching(keys);
      // Esc that continues no sequence abandons the pending keys and does nothing else: neither the shorter binding
      // nor Esc's own action runs, so a stray prefix before Esc cannot close what the page has open.
      if (!matches.length && pending.length && stroke.key === "Escape") {
        reset();
        return { kind: "cancelled" };
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
