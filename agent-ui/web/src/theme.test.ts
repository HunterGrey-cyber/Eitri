import { describe, expect, it, vi } from "vitest";
import { applyTheme } from "./theme";

function recorder() {
  const set = new Map<string, string>();
  return { set, target: { setProperty: (name: string, value: string) => void set.set(name, value) } };
}

describe("applyTheme", () => {
  it("sets every --nv-* variable it is given", () => {
    const { set, target } = recorder();
    const count = applyTheme({ "--nv-bg": "#faf4ed", "--nv-font-mono": '"Maple Mono", monospace' }, target);
    expect(count).toBe(2);
    expect(set.get("--nv-bg")).toBe("#faf4ed");
    expect(set.get("--nv-font-mono")).toBe('"Maple Mono", monospace');
  });

  it("refuses any name outside the --nv- namespace", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { set, target } = recorder();
    const count = applyTheme({ color: "red", "--other": "x", "--nv-Bad": "x", "--nv-ok": "#00ff00" }, target);
    expect(count).toBe(1);
    expect([...set.keys()]).toEqual(["--nv-ok"]);
    expect(warn).toHaveBeenCalledTimes(3);
    warn.mockRestore();
  });
});
