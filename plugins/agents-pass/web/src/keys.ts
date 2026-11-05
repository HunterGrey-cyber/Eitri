// The pass's keys: the shared grammar the agents kit declares (j, k, Enter, c, x, /, Esc), plus `n` for the next
// stop (or the first, with no round at a stop), and Esc that leaves a running round (back to where it started)
// before it gives the keys back.
import { running, type PassStore } from "./strip";

export type KeyRow = { action: string; keys: string[]; context: string; desc: string };
/** The part of `window.eitriKeys` the agents kit installs through. */
export type KeysKit = {
  install(opts: { rows: KeyRow[]; run(action: string, ev: KeyboardEvent): boolean; [more: string]: unknown }):
    Promise<() => void>;
  message(text: string): void;
  [more: string]: unknown;
};

export const PASS_ROWS: KeyRow[] = [
  { action: "pass.next", keys: ["n"], context: "Agents", desc: "Next agent that waits" },
];

/** A keys kit that installs the pass's own rows beside whatever the agents kit installs through it. */
export function passKeys(kit: KeysKit, store: PassStore, call: (verb: string, args: unknown) => void): KeysKit {
  return {
    ...kit,
    install: (opts) =>
      kit.install({
        ...opts,
        rows: [...opts.rows, ...PASS_ROWS],
        run(action, ev) {
          if (action === "pass.next") {
            call("agents.round", running(store.round(), store.rows()) ? { op: "next" } : { op: "start" });
            return true;
          }
          if (action === "agents.escape" && running(store.round(), store.rows())) {
            call("agents.round", { op: "leave" });
            return true;
          }
          return opts.run(action, ev);
        },
      }),
  };
}
