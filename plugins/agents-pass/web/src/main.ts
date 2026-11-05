// The page: the agents kit's store drawn as one line, its shared keys with the pass's own.
import "./pass.css";
import { mountStrip, type PassRow, type PassStore } from "./strip";
import { passKeys, type KeysKit } from "./keys";

type Bridge = { call(verb: string, args: unknown): Promise<unknown> };
type AgentsKit = {
  store: PassStore;
  glyph(row: PassRow): string;
  start(): Promise<void>;
  bindKeys(opts: { order: () => string[] }): Promise<() => void>;
};
type Host = Window & { eitriAgents?: AgentsKit; eitriKeys?: KeysKit; eitriPlugin?: Bridge };

async function main(): Promise<void> {
  const host = window as Host;
  const root = document.getElementById("pass");
  const { eitriAgents: agents, eitriKeys: keys, eitriPlugin: bridge } = host;
  if (!root || !agents || !keys || !bridge) return;
  const say = (error: unknown) => keys.message(error instanceof Error ? error.message : String(error));
  const call = (verb: string, args: unknown) => void bridge.call(verb, args).catch(say);
  // The agents kit installs its keys through `window.eitriKeys`; this page's `n` and Esc ride along.
  host.eitriKeys = passKeys(keys, agents.store, call);
  const strip = mountStrip(root, { store: agents.store, glyph: agents.glyph });
  try {
    await agents.start();
    await agents.bindKeys({ order: strip.order });
  } catch (error) {
    // Agents off, or not served here: the strip stays empty.
    console.warn(error instanceof Error ? error.message : String(error));
  }
}

void main();
