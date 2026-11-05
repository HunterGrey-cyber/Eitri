import { createStore } from "./store";
import type { Bridge } from "./store";
import { glyph, word } from "./glyphs";
import { bindKeys as bind } from "./keys";
import type { BindOptions, Hello } from "./keys";

export type { Row, Round, Project, Selection, View, Store } from "./store";
export type { BindOptions } from "./keys";

const { store, apply } = createStore();
let started: Promise<void> | undefined;
let hello: Hello;
function bridge(): Bridge {
  const host = (window as Window & { eitriPlugin?: Bridge }).eitriPlugin;
  if (!host) throw new Error("not connected to Eitri");
  return host;
}
export function start(): Promise<void> {
  if (started) return started;
  started = (async () => {
    const removals: Array<() => void> = [];
    const pending: Array<[string, unknown]> = [];
    let ready = false;
    try {
      const host = bridge();
      hello = await host.call("host.hello", {}) as Hello;
      for (const event of ["agents.row", "agents.gone", "agents.reset", "agents.selection", "agents.round"]) {
        removals.push(host.on(event, data => {
          if (ready) apply(event, data);
          else pending.push([event, data]);
        }));
      }
      apply("agents.reset", await host.call("agents.watch", {}));
      for (const [event, data] of pending) apply(event, data);
      ready = true;
    } catch (error) {
      removals.forEach(remove => remove());
      throw error;
    }
  })().catch(error => { started = undefined; throw error; });
  return started;
}
export async function bindKeys(opts: BindOptions): Promise<() => void> {
  await start();
  return bind(opts, store, bridge(), hello);
}
export { store, glyph, word };
export const eitriAgents = Object.freeze({ version: 1 as const, store, glyph, word, bindKeys, start });

declare global {
  interface Window {
    eitriAgents: typeof eitriAgents;
  }
}

window.eitriAgents = eitriAgents;
