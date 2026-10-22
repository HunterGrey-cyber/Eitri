import type { PanelBinding, PanelTable } from "./keymap";

/** One binding, defaulting its `desc` to the action name and its `source` to "default" -- shared
 *  by every test that needs a `PanelTable` (`leader.test.ts`, `App.test.tsx` and
 *  `KeymapOverlay.test.tsx` build on the same fixture rather than each keeping its own copy). */
export const binding = (keys: string[], action: PanelBinding["action"], desc: string = action): PanelBinding => ({
  keys,
  action,
  desc,
  source: "default",
});

/** A small panel table covering a one-key leaf (`H`/`L`), a two-key binding through one of
 *  `resolveKey`'s own reserved prefixes (`[ b`), three leader sequences (two of them sharing a
 *  group label), and the two group labels themselves -- exactly what `leader.test.ts` and
 *  `App.test.tsx`/`KeymapOverlay.test.tsx` exercise. */
export const TABLE: PanelTable = {
  leader: " ",
  leaderLabel: "Space",
  leaderSource: "default",
  timeoutlen: 300,
  timeout: true,
  bindings: [
    binding(["H"], "tab.prev"),
    binding(["L"], "tab.next"),
    binding(["[", "b"], "tab.prev"),
    binding(["<leader>", "b", "d"], "tab.close", "close tab"),
    binding(["<leader>", "b", "b"], "tab.last", "last tab"),
    binding(["<leader>", "f", "n"], "tab.new", "new tab"),
    binding(["<leader>", "m"], "mode.cycle", "mode"),
  ],
  groups: [
    { keys: ["<leader>", "b"], label: "+tab" },
    { keys: ["<leader>", "f"], label: "+new" },
  ],
};
