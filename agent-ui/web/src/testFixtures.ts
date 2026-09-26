import type { PanelBinding, PanelTable } from "./keymap";

/** Shared test binding with `desc` defaulting to the action name and `source` set to "default".
 *  Tests using a `PanelTable` reuse this fixture instead of keeping separate copies. */
export const binding = (keys: string[], action: PanelBinding["action"], desc: string = action): PanelBinding => ({
  keys,
  action,
  desc,
  source: "default",
});

/** A small panel table with one-key leaves (`H`/`L`), a two-key reserved-prefix binding (`[ b`),
 *  three leader sequences, two sharing a group label, and both group labels. It is shared by
 *  `leader.test.ts`, `App.test.tsx` and `KeymapOverlay.test.tsx`. */
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
