// @vitest-environment node
import { describe, expect, it, vi } from "vitest";
import { createDiagramRenderer } from "./diagram-renderer";
import * as entry from "./diagram-renderer";
import { MERMAID_CONFIG, themeVariables } from "./render/mermaid-config";

describe("isolated diagram entry", () => {
  it("imports the config without a window and configures once per host theme", async () => {
    expect(MERMAID_CONFIG.securityLevel).toBe("strict");
    expect(themeVariables({"--nv-bg":"#fff"}).background).toBe("#fff");
    const api = { initialize: vi.fn(), render: vi.fn().mockResolvedValue({svg:"<svg/>"}) };
    const render = createDiagramRenderer(async () => api);
    expect(JSON.parse(await render("A", "{}"))).toEqual({svg:"<svg/>"});
    await render("B", "{}"); expect(api.initialize).toHaveBeenCalledTimes(1);
    expect(api.initialize.mock.calls[0][0]).toMatchObject({maxTextSize:20000, maxEdges:250});
    await render("C", '{"--nv-bg":"#fff"}'); expect(api.initialize).toHaveBeenCalledTimes(2);
    api.render.mockRejectedValueOnce(new EvalError("eval refused"));
    expect(JSON.parse(await render("C", "{}"))).toMatchObject({later:true});
    api.render.mockRejectedValueOnce({message:"bad syntax"});
    expect(JSON.parse(await render("C", "{}"))).toEqual({error:"bad syntax",later:false});
  });
});

it("a failed library load is transient and the next call retries", async () => {
  const api = { initialize: vi.fn(), render: vi.fn().mockResolvedValue({ svg: "<svg/>" }) };
  const load = vi.fn().mockRejectedValueOnce(new Error("missing module")).mockResolvedValue(api);
  const render = createDiagramRenderer(load);
  expect(JSON.parse(await render("A", "{}"))).toEqual({ error: "the renderer stopped", later: false, transient: true, startup: true });
  expect(JSON.parse(await render("A", "{}"))).toEqual({ svg: "<svg/>" });
  expect(load).toHaveBeenCalledTimes(2);
});

it("acknowledges library readiness before starting any diagram and retries failed imports", async () => {
  const api = { initialize: vi.fn(), render: vi.fn().mockResolvedValue({ svg: "<svg/>" }) };
  let resolve!: (value: typeof api) => void;
  const load = vi.fn().mockRejectedValueOnce(new Error("missing module"))
    .mockImplementationOnce(() => new Promise<typeof api>((done) => { resolve = done; }));
  const render = createDiagramRenderer(load);
  await expect(render.ready()).rejects.toThrow("missing module");
  const ready = render.ready();
  expect(api.render).not.toHaveBeenCalled();
  resolve(api);
  await expect(ready).resolves.toBe(true);
  expect(api.initialize).not.toHaveBeenCalled();
  await render("A", "{}");
  expect(load).toHaveBeenCalledTimes(2);
  expect(api.render).toHaveBeenCalledTimes(1);
});

it("signals the host only after installing eitriRender and loading the library", async () => {
  const api = { initialize: vi.fn(), render: vi.fn().mockResolvedValue({ svg: "<svg/>" }) };
  let resolve!: (value: typeof api) => void;
  const render = createDiagramRenderer(() => new Promise<typeof api>((done) => { resolve = done; }));
  const page: { eitriRender?: typeof render } = {};
  const signal = vi.fn(() => { expect(page.eitriRender).toBe(render); });
  const ready = entry.installDiagramRenderer(page, signal, render);
  expect(page.eitriRender).toBe(render);
  expect(signal).not.toHaveBeenCalled();
  expect(api.render).not.toHaveBeenCalled();
  resolve(api);
  await ready;
  expect(signal).toHaveBeenCalledTimes(1);
  expect(signal).toHaveBeenCalledWith("ready");
  expect(api.render).not.toHaveBeenCalled();
});

it("signals a failed import without acknowledging readiness", async () => {
  const page = {};
  const signal = vi.fn();
  await entry.installDiagramRenderer(page, signal, createDiagramRenderer(async () => { throw new Error("missing"); }));
  expect(signal).toHaveBeenCalledTimes(1);
  expect(signal).toHaveBeenCalledWith("failed");
});
