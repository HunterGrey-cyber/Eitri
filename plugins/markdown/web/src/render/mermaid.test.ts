import { afterEach, describe, expect, it, vi } from "vitest";
import { CACHE_CHARS, CACHE_ENTRIES, createMermaid, MERMAID_CONFIG, mermaidPlaceholder, themeVariables, type MermaidApi } from "./mermaid";

function fakeApi(render: (id: string, text: string) => Promise<{ svg: string }>) {
  const api = { initialize: vi.fn(), render: vi.fn(render) } satisfies MermaidApi;
  const load = vi.fn(async () => api);
  return { api, load };
}

function live(...sources: string[]): HTMLElement {
  const root = document.createElement("main");
  root.innerHTML = sources.map((s) => mermaidPlaceholder(s, "n")).join("") + "<p>text</p>";
  document.body.append(root);
  return root;
}

const svg = (label: string) => `<svg class="flow"><g class="node"><text>${label}</text></g></svg>`;
const VARS = { "--nv-bg": "#101010", "--nv-fg": "#e0e0e0", "--nv-surface": "#202020", "--nv-color-scheme": "dark",
  "--nv-font-prose": "\"IBM Plex Sans\", sans-serif", "--nv-syn-function": "#88aaff" };

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

describe("the diagram runner", () => {
  it("never loads Mermaid for a document without a diagram", async () => {
    const { load } = fakeApi(async () => ({ svg: svg("x") }));
    const runner = createMermaid(load);
    const root = document.createElement("main");
    root.innerHTML = "<p>no diagrams</p>";
    expect(runner.fill(root)).toBe(0);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await new Promise((r) => setTimeout(r, 20));
    expect(load).not.toHaveBeenCalled();
    expect(onDone).not.toHaveBeenCalled();
  });

  it("loads Mermaid once, configures it strictly in the theme, and renders one diagram at a time", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    const order: string[] = [];
    const { api, load } = fakeApi(async (_id, text) => {
      order.push(`start ${text}`);
      if (text === "A") await gate;
      order.push(`end ${text}`);
      return { svg: svg(text) };
    });
    const runner = createMermaid(load);
    runner.setTheme(VARS);
    const root = live("A", "B");
    expect(runner.fill(root)).toBe(0);
    expect(root.querySelectorAll(".mdp-diagram-pending").length).toBe(2);
    expect(root.querySelector(".mdp-diagram-pending")!.textContent).toBe("rendering diagram");
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(order).toEqual(["start A"]));
    release();
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(2));
    expect(order).toEqual(["start A", "end A", "start B", "end B"]);
    expect(load).toHaveBeenCalledTimes(1);
    expect(api.initialize).toHaveBeenCalledTimes(1);
    const config = api.initialize.mock.calls[0][0] as Record<string, unknown>;
    expect(config).toMatchObject({ ...MERMAID_CONFIG, themeVariables: themeVariables(VARS) });
    expect(config).toMatchObject({ securityLevel: "strict", startOnLoad: false, htmlLabels: false,
      flowchart: { htmlLabels: false }, maxTextSize: 50000, theme: "base" });
    const drawn = [...root.querySelectorAll("[data-eitri-mermaid]")];
    expect(drawn.map((d) => d.querySelector("text")?.textContent)).toEqual(["A", "B"]);
    expect(drawn.every((d) => !d.classList.contains("mdp-diagram-pending") && d.hasAttribute("data-mdp-key"))).toBe(true);
  });

  it("builds the theme from the editor's colours", () => {
    expect(themeVariables(VARS)).toMatchObject({ background: "#101010", textColor: "#e0e0e0", primaryColor: "#202020",
      mainBkg: "#202020", darkMode: true, fontFamily: "\"IBM Plex Sans\", sans-serif", pie1: "#88aaff", cScale0: "#88aaff" });
    expect(themeVariables({ "--nv-color-scheme": "light" }).darkMode).toBe(false);
  });

  it("sanitises what Mermaid returns", async () => {
    const { load } = fakeApi(async () => ({
      svg: '<svg onload="alert(1)"><g class="node"><text>x</text></g><script>alert(1)</script><foreignObject><div>h</div></foreignObject></svg>',
    }));
    const runner = createMermaid(load);
    const root = live("A");
    runner.fill(root);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalled());
    const out = root.querySelector("[data-eitri-mermaid] svg")!;
    expect(out.hasAttribute("onload")).toBe(false);
    expect(out.querySelector("script, foreignObject")).toBeNull();
    expect(out.querySelector("g.node text")!.textContent).toBe("x");
  });

  it("draws a known diagram at once from its cache, and renders again after the theme changes", async () => {
    const { api, load } = fakeApi(async (_id, text) => ({ svg: svg(text) }));
    const runner = createMermaid(load);
    runner.setTheme(VARS);
    const first = live("A");
    runner.fill(first);
    const onDone = vi.fn();
    runner.renderPending(first, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
    const again = live("A");
    expect(runner.fill(again)).toBe(1);
    expect(again.querySelector("text")!.textContent).toBe("A");
    runner.setTheme(VARS);
    expect(runner.fill(live("A"))).toBe(1);
    runner.setTheme({ ...VARS, "--nv-bg": "#fafafa", "--nv-color-scheme": "light" });
    const third = live("A");
    expect(runner.fill(third)).toBe(0);
    runner.renderPending(third, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(2));
    expect(api.render).toHaveBeenCalledTimes(2);
    expect(api.initialize).toHaveBeenCalledTimes(2);
    expect((api.initialize.mock.calls[1][0] as { themeVariables: { background: string } }).themeVariables.background).toBe("#fafafa");
  });

  it("keeps the diagrams used last, up to its entry cap", async () => {
    const { api, load } = fakeApi(async (_id, text) => ({ svg: svg(text) }));
    const runner = createMermaid(load);
    const drawn = async (source: string) => {
      const root = live(source);
      runner.fill(root);
      const onDone = vi.fn();
      runner.renderPending(root, onDone);
      await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
      root.remove();
    };
    // An edit inside a diagram is a new source each time: typing one keeps CACHE_ENTRIES of them at most.
    for (let i = 0; i < CACHE_ENTRIES; i++) await drawn(`D${i}`);
    expect(runner.fill(live("D0"))).toBe(1);
    await drawn(`D${CACHE_ENTRIES}`);
    expect(runner.fill(live("D0"))).toBe(1);
    expect(runner.fill(live("D1"))).toBe(0);
    expect(runner.fill(live(`D${CACHE_ENTRIES}`))).toBe(1);
    expect(api.render).toHaveBeenCalledTimes(CACHE_ENTRIES + 1);
  });

  it("keeps at most CACHE_CHARS of outcomes", async () => {
    // Failed diagrams, whose message is kept as it is: the same count as an SVG's text, without parsing one this large.
    const { load } = fakeApi(async (_id, text) => { throw { str: `${text} ${"x".repeat(CACHE_CHARS / 2)}` }; });
    const runner = createMermaid(load);
    for (const source of ["A", "B", "C"]) {
      const root = live(source);
      runner.fill(root);
      const onDone = vi.fn();
      runner.renderPending(root, onDone);
      await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
      root.remove();
    }
    expect(runner.fill(live("A"))).toBe(0);
    expect(runner.fill(live("B"))).toBe(0);
    expect(runner.fill(live("C"))).toBe(1);
  });

  it("never evicts a diagram of the document, however many it holds", async () => {
    const { api, load } = fakeApi(async (_id, text) => ({ svg: svg(text) }));
    const runner = createMermaid(load);
    const count = CACHE_ENTRIES + 6;
    const sources = Array.from({ length: count }, (_, i) => `D${i}`);
    const render = async (list: string[]) => {
      const root = live(...list);
      const filled = runner.fill(root);
      const onDone = vi.fn();
      runner.renderPending(root, onDone);
      await vi.waitFor(() => expect(root.querySelector(".mdp-diagram-pending")).toBeNull());
      root.remove();
      return filled;
    };
    // An edit outside the diagrams: the same diagrams, rendered again, are all drawn from the cache.
    expect(await render(sources)).toBe(0);
    expect(await render(sources)).toBe(count);
    expect(await render(sources)).toBe(count);
    expect(api.render).toHaveBeenCalledTimes(count);
    // An edit inside one diagram: only that one is drawn, and its old revision is what the bound lets go.
    const edited = ["D0 edited", ...sources.slice(1)];
    expect(await render(edited)).toBe(count - 1);
    expect(await render(edited)).toBe(count);
    expect(api.render).toHaveBeenCalledTimes(count + 1);
    expect(runner.fill(live("D0"))).toBe(0);
  });

  it("keeps a document's diagrams past CACHE_CHARS", async () => {
    const { api, load } = fakeApi(async (_id, text) => { throw { str: `${text} ${"x".repeat(CACHE_CHARS / 2)}` }; });
    const runner = createMermaid(load);
    const root = live("A", "B", "C");
    runner.fill(root);
    runner.renderPending(root, vi.fn());
    await vi.waitFor(() => expect(root.querySelector(".mdp-diagram-pending")).toBeNull());
    expect(runner.fill(live("A", "B", "C"))).toBe(3);
    expect(api.render).toHaveBeenCalledTimes(3);
  });

  it("draws a diagram again in the new theme when the theme changes while it renders", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    let calls = 0;
    const { api, load } = fakeApi(async (_id, text) => {
      calls++;
      if (calls === 1) await gate;
      return { svg: svg(`${text}${calls}`) };
    });
    const runner = createMermaid(load);
    runner.setTheme(VARS);
    const root = live("A");
    runner.fill(root);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(api.render).toHaveBeenCalledTimes(1));
    runner.setTheme({ ...VARS, "--nv-bg": "#fafafa", "--nv-color-scheme": "light" });
    // The page updated in place: the same waiting element, with the same source, is still there.
    runner.renderPending(root, onDone);
    release();
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
    await new Promise((r) => setTimeout(r, 20));
    expect(onDone).toHaveBeenCalledTimes(1);
    expect(api.render).toHaveBeenCalledTimes(2);
    expect(api.initialize).toHaveBeenCalledTimes(2);
    expect((api.initialize.mock.calls[1][0] as { themeVariables: { background: string } }).themeVariables.background).toBe("#fafafa");
    const el = root.querySelector<HTMLElement>("[data-eitri-mermaid]")!;
    expect(el.classList.contains("mdp-diagram-pending")).toBe(false);
    expect(el.querySelector("text")!.textContent).toBe("A2");
  });

  it("shows a failing diagram's source and Mermaid's message, and the code with a note when eval is refused", async () => {
    const { load } = fakeApi(async (_id, text) => {
      if (text === "bad") throw { str: "Parse error on line 1" };
      throw new EvalError("Refused to evaluate a string as JavaScript because 'unsafe-eval' is not an allowed source");
    });
    const runner = createMermaid(load);
    const root = live("bad", "evil");
    runner.fill(root);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(2));
    const [bad, evil] = root.querySelectorAll("[data-eitri-mermaid]");
    expect(bad.classList.contains("mdp-box")).toBe(true);
    expect(bad.querySelector("pre code")!.textContent).toBe("bad");
    expect(bad.querySelector("p")!.textContent).toBe("Parse error on line 1");
    expect(evil.querySelector("pre code")!.textContent).toBe("evil");
    expect(evil.querySelector("p")!.textContent).toBe("This diagram needs a later version of the preview.");
  });

  it("sizes a waiting diagram from the height the diagram in its place last had", async () => {
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({ height: 120 } as DOMRect);
    const { load } = fakeApi(async (_id, text) => ({ svg: svg(text) }));
    const runner = createMermaid(load);
    const fresh = live("Z");
    runner.fill(fresh);
    expect((fresh.querySelector("[data-eitri-mermaid]") as HTMLElement).style.minHeight).toBe("");
    fresh.remove();
    const first = live("A", "B");
    runner.fill(first);
    const onDone = vi.fn();
    runner.renderPending(first, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(2));
    expect((first.querySelector("[data-eitri-mermaid]") as HTMLElement).style.minHeight).toBe("");
    const edited = live("A2", "B");
    expect(runner.fill(edited)).toBe(1);
    const [waiting, drawn] = edited.querySelectorAll<HTMLElement>("[data-eitri-mermaid]");
    expect(waiting.classList.contains("mdp-diagram-pending")).toBe(true);
    expect(waiting.style.minHeight).toBe("120px");
    expect(drawn.style.minHeight).toBe("");
  });

  it("tries to load Mermaid again for the next diagram after a load fails", async () => {
    const api = { initialize: vi.fn(), render: vi.fn(async (_id: string, text: string) => ({ svg: svg(text) })) };
    const load = vi.fn().mockRejectedValueOnce(new Error("load failed")).mockResolvedValue(api);
    const runner = createMermaid(load);
    const root = live("A", "B");
    runner.fill(root);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledTimes(2));
    const [a, b] = root.querySelectorAll("[data-eitri-mermaid]");
    expect(a.querySelector("p")!.textContent).toBe("load failed");
    expect(b.querySelector("text")!.textContent).toBe("B");
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("leaves a diagram the page replaced while it waited, and draws the new page's", async () => {
    const { api, load } = fakeApi(async (_id, text) => ({ svg: svg(text) }));
    const runner = createMermaid(load);
    const old = live("A");
    runner.fill(old);
    const first = vi.fn();
    runner.renderPending(old, first);
    old.remove();
    const next = live("B");
    runner.fill(next);
    const second = vi.fn();
    runner.renderPending(next, second);
    await vi.waitFor(() => expect(second).toHaveBeenCalledTimes(1));
    expect(next.querySelector("text")!.textContent).toBe("B");
    expect(api.render.mock.calls.map((c) => c[1])).toEqual(["B"]);
    expect(first).not.toHaveBeenCalled();
  });

  it("traces loading and each diagram", async () => {
    const info = vi.spyOn(console, "info").mockImplementation(() => {});
    const { load } = fakeApi(async () => ({ svg: svg("x") }));
    const runner = createMermaid(load);
    const root = live("A");
    runner.fill(root);
    const onDone = vi.fn();
    runner.renderPending(root, onDone);
    await vi.waitFor(() => expect(onDone).toHaveBeenCalled());
    const lines = info.mock.calls.map((c) => String(c[0])).filter((s) => s.startsWith("[page-trace] diagram "));
    expect(lines).toEqual(['[page-trace] diagram {"state":"loaded"}', '[page-trace] diagram {"state":"rendered"}']);
  });
});
