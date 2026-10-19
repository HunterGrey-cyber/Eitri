import { readFileSync } from "node:fs";
import { JSDOM } from "jsdom";
import { afterEach, describe, expect, it } from "vitest";

const bridge = readFileSync(new URL("../../../core/src/plugin/bridge.js", import.meta.url), "utf8");
const windows: JSDOM[] = [];

function page(random: number) {
  const dom = new JSDOM("", { runScripts: "outside-only" });
  windows.push(dom);
  const posted: { id: number; verb: string }[] = [];
  const win = dom.window;
  win.Math.random = () => random;
  win.TextEncoder = TextEncoder;
  win.webkit = { messageHandlers: { eitriPlugin: {
    postMessage: (text: string) => posted.push(JSON.parse(text)),
  } } };
  win.eval(bridge);
  return {
    call: (verb: string) => win.eitriPlugin.call(verb, {}),
    reply: (id: number, result: unknown) => win.__eitriPluginDispatch(JSON.stringify({ v: 1, re: id, ok: true, result })),
    posted,
  };
}

afterEach(() => { windows.splice(0).forEach((dom) => dom.window.close()); });

describe("plugin bridge document replies", () => {
  it.each([0, 12])("ignores another document's answer after %i earlier calls", async (earlier) => {
    const a = page(0.25);
    const b = page(0.75);
    for (let i = 0; i < earlier; i++) {
      for (const doc of [a, b]) {
        const pending = doc.call("state.read");
        doc.reply(doc.posted[i].id, { value: i });
        await pending;
      }
    }
    const oldRead = a.call("state.read");
    const hello = b.call("host.hello");
    let settled = false;
    void hello.then(() => { settled = true; }, () => { settled = true; });
    b.reply(a.posted[earlier].id, { value: "another page's state" });
    await Promise.resolve();
    expect(settled).toBe(false);
    const answer = { host: "eitri", keys_kit: 1 };
    b.reply(b.posted[earlier].id, answer);
    await expect(hello).resolves.toEqual(answer);
    a.reply(a.posted[earlier].id, { value: "own state" });
    await expect(oldRead).resolves.toEqual({ value: "own state" });
  });

  it.each([0, 1 - Number.EPSILON])("keeps successive ids safe integers at random=%s", async (random) => {
    const doc = page(random);
    for (let i = 0; i < 3; i++) {
      const pending = doc.call("state.read");
      const id = doc.posted[i].id;
      expect(Number.isSafeInteger(id)).toBe(true);
      expect(id).toBeGreaterThan(0);
      if (i > 0) expect(id).toBe(doc.posted[i - 1].id + 1);
      doc.reply(id, { value: i });
      await expect(pending).resolves.toEqual({ value: i });
    }
  });
});
