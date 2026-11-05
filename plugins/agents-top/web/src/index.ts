import type { eitriAgents } from "../../../../plugin-kit/agents/dist/agents";
import type { Bridge } from "../../../../plugin-kit/agents/dist/store";
import type { KeysKit } from "../../../../plugin-kit/agents/dist/keys";
import { createTable } from "./table";
import "./style.css";

type HostWindow = Window & { eitriAgents: typeof eitriAgents; eitriKeys: KeysKit; eitriPlugin: Bridge };
export async function start(root: HTMLElement, host = window as unknown as HostWindow): Promise<() => void> {
  const kit = host.eitriAgents;
  await kit.start();
  const table = createTable(root, kit, key => {
    void host.eitriPlugin.call("agents.select", { key }).catch(error => host.eitriKeys.message(String(error)));
  });
  // The keys kit has one installation. Extend the agents installation so its grammar and host key settings stay together.
  const install = host.eitriKeys.install;
  host.eitriKeys.install = options => install({
    ...options,
    rows: [...options.rows,
      { action: "top.sort", keys: ["s"], context: "Agents", desc: "Sort agents" },
      { action: "top.reverse", keys: ["S"], context: "Agents", desc: "Reverse sort" }],
    run: (action, event) => action === "top.sort" ? table.handleKey("s")
      : action === "top.reverse" ? table.handleKey("S") : options.run(action, event),
  });
  try {
    const remove = await kit.bindKeys({ order: table.order, onFilter: table.setFilter });
    return () => { remove(); table.destroy(); };
  } catch (error) {
    table.destroy();
    throw error;
  } finally {
    host.eitriKeys.install = install;
  }
}
const root = document.getElementById("agents");
if (root) {
  void start(root).catch(error => {
    root.textContent = (error as { code?: string }).code === "disabled" ? "Agents are off" : "Could not load agents";
  });
}
