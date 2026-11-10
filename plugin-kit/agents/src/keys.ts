import type { Bridge, Store } from "./store";

export type BindOptions = { order: () => string[]; onFilter?: (q: string) => void };
export type Hello = {
  capabilities: string[];
  timeoutlen?: number;
  prefix?: string | null;
  keys_settings?: unknown;
};
type KeyRow = { action: string; keys: string[]; context: string; desc: string };
export type KeysKit = {
  install(opts: {
    rows: KeyRow[];
    run(action: string, ev: KeyboardEvent): boolean;
    initial?: { timeoutlen: number; hasKeys: boolean; prefix?: string | null; settings?: unknown };
  }): Promise<() => void>;
  message(text: string): void;
};

export async function bindKeys(opts: BindOptions, store: Store, bridge: Bridge, hello: Hello): Promise<() => void> {
  const keys = (window as Window & { eitriKeys?: KeysKit }).eitriKeys;
  if (!keys) throw new Error("the Eitri keys kit is not loaded");
  let prompt: HTMLInputElement | undefined;
  let previous: Element | null = null;
  let query = "";
  const close = () => {
    prompt?.remove();
    prompt = undefined;
    if (previous instanceof HTMLElement) previous.focus();
    previous = null;
  };
  const ask = (verb: string, args: unknown) => {
    void bridge.call(verb, args).catch(error => keys.message(error instanceof Error ? error.message : String(error)));
  };
  const bindings = [
    ["agents.next", "j", "Next agent"], ["agents.prev", "k", "Previous agent"],
    ["agents.open", "<CR>", "Open agent"], ["agents.new", "c", "New agent"],
    ["agents.stop", "x", "Ask to stop agent"], ["agents.filter", "/", "Filter agents"],
    ["agents.escape", "<Esc>", "Focus editor"],
  ];
  const remove = await keys.install({
    rows: bindings.map(([action, key, desc]) => ({ action, keys: [key], context: "Agents", desc })),
    initial: { timeoutlen: hello.timeoutlen ?? 1000, hasKeys: true, prefix: hello.prefix, settings: hello.keys_settings },
    run(action) {
      const order = opts.order();
      const selected = store.selection();
      const row = selected === null || !order.includes(selected) ? undefined : store.byKey(selected);
      switch (action) {
        case "agents.next":
        case "agents.prev": {
          if (!order.length) return true;
          const at = selected === null ? -1 : order.indexOf(selected);
          const next = at < 0 ? (action === "agents.next" ? 0 : order.length - 1)
            : Math.max(0, Math.min(order.length - 1, at + (action === "agents.next" ? 1 : -1)));
          ask("agents.select", { key: order[next] });
          return true;
        }
        case "agents.open":
          if (row?.class === "needs") ask("agents.round", { op: "start", key: row.key });
          else if (row) ask("agents.focus", { id: row.id });
          return true;
        case "agents.new":
          if (row) ask("agents.new", { project: row.project.key });
          return true;
        case "agents.stop":
          if (row) ask("agents.stop", { id: row.id });
          return true;
        case "agents.escape":
          if (hello.capabilities.includes("layout.focus")) ask("layout.focus", { target: "editor" });
          return true;
        case "agents.filter":
          if (!opts.onFilter) return true;
          if (!prompt) {
            previous = document.activeElement;
            prompt = document.createElement("input");
            prompt.type = "search";
            prompt.setAttribute("aria-label", "Filter agents");
            prompt.placeholder = "Filter agents";
            prompt.value = query;
            prompt.style.cssText = "position:fixed;bottom:0;left:0;right:0;z-index:2147483647;" +
              "background:var(--nv-bg,#202020);color:var(--nv-fg,#eeeeee);font:inherit";
            prompt.addEventListener("input", () => { query = prompt!.value; opts.onFilter!(query); });
            prompt.addEventListener("keydown", event => {
              if (event.key === "Escape" || event.key === "Enter") {
                event.preventDefault();
                event.stopPropagation();
                close();
              }
            });
            document.body.append(prompt);
          }
          prompt.focus();
          return true;
        default: return false;
      }
    },
  });
  return () => { close(); remove(); };
}
