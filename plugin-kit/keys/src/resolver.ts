import { notationOf, parseNotation, type KeySeq, type KeyStroke } from "./notation";
import type { Row } from "./rows";

export type Resolution = { kind: "action"; action: string } | { kind: "pending"; prefix: string } | { kind: "none" };

export function createResolver(rows: Row[], timeoutlen: () => number, now: () => number) {
  const bindings = rows.flatMap((row) => row.keys.flatMap((key) => {
    const seq = parseNotation(key);
    return seq ? [{ seq, action: row.action }] : [];
  }));
  let pending: KeySeq = [];
  let last = 0;
  const reset = () => { pending = []; last = 0; };

  return {
    reset,
    feed(stroke: KeyStroke): Resolution {
      const time = now();
      if (pending.length && time - last > timeoutlen()) {
        reset();
        return { kind: "none" };
      }
      pending.push(stroke);
      const matches = bindings.filter(({ seq }) => seq.length >= pending.length && pending.every((part, i) =>
        part.key === seq[i].key && part.ctrl === seq[i].ctrl && part.shift === seq[i].shift));
      const exact = matches.find(({ seq }) => seq.length === pending.length);
      if (exact) {
        reset();
        return { kind: "action", action: exact.action };
      }
      if (matches.length) {
        last = time;
        return { kind: "pending", prefix: pending.map(notationOf).join("") };
      }
      reset();
      return { kind: "none" };
    },
  };
}
