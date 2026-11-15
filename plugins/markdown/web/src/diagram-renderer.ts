import { MERMAID_CONFIG, themeVariables } from "./render/mermaid-config";
import type { MermaidApi } from "./render/mermaid";

const MERMAID_URL = "./mermaid.js";

/** A renderer without a bridge: the caller supplies text and the host's theme, and receives only JSON text. */
export function createDiagramRenderer(load: () => Promise<MermaidApi> = async () =>
  ((await import(MERMAID_URL)) as { default: MermaidApi }).default) {
  let api: Promise<MermaidApi> | null = null;
  let configured: string | null = null;
  let counter = 0;
  const ready = () => api ??= load().catch((e: unknown) => { api = null; throw e; });
  const render = async (source: string, varsJson: string): Promise<string> => {
    let loaded = false;
    try {
      const mermaid = await ready();
      loaded = true;
      if (configured !== varsJson) {
        mermaid.initialize({ ...MERMAID_CONFIG, maxTextSize: 20000, maxEdges: 250,
          themeVariables: themeVariables(JSON.parse(varsJson) as Record<string, string>) });
        configured = varsJson;
      }
      return JSON.stringify({ svg: (await mermaid.render(`d${++counter}`, source)).svg });
    } catch (e) {
      if (!loaded) return JSON.stringify({ error: "the renderer stopped", later: false, transient: true, startup: true });
      const shaped = e as { message?: unknown; str?: unknown } | null;
      const error = typeof shaped?.message === "string" ? shaped.message
        : typeof shaped?.str === "string" ? shaped.str : String(e);
      return JSON.stringify({ error, later: e instanceof EvalError || /unsafe-eval/.test(error) });
    }
  };
  return Object.assign(render, { ready: async () => { await ready(); return true; } });
}

type Render = ReturnType<typeof createDiagramRenderer>;
export async function installDiagramRenderer(
  page: { eitriRender?: Render }, signal: (message: "ready" | "failed") => void,
  render: Render = createDiagramRenderer(),
): Promise<void> {
  page.eitriRender = render;
  try {
    await render.ready();
  } catch {
    signal("failed");
    return;
  }
  signal("ready");
}

if (typeof window !== "undefined") {
  const page = window as unknown as Window & {
    eitriRender?: Render;
    webkit: { messageHandlers: { eitriDiagram: { postMessage: (message: string) => void } } };
  };
  void installDiagramRenderer(page, (message) => page.webkit.messageHandlers.eitriDiagram.postMessage(message));
}
