// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { KeymapOverlay } from "./KeymapOverlay";
import { BROWSE_KEYS, EMPTY_PANEL_TABLE, INPUT_KEYS } from "../keymap";
import type { KeyHelp, PanelTable } from "../keymap";
import { TABLE } from "../testFixtures";

afterEach(cleanup);

const WINDOW: KeyHelp[] = [{ keys: "F11", what: "Fullscreen" }];
const PREFIX: KeyHelp[] = [
  { keys: "Ctrl+b f", what: "HINT: jump anywhere in the window" },
  { keys: "Ctrl+b %", what: "Then a module key (e / a / t): open it right of this one, or move it there" },
];

function overlay(onClose = () => {}, panel: PanelTable = EMPTY_PANEL_TABLE) {
  return render(<KeymapOverlay onClose={onClose} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={panel} />);
}

describe("KeymapOverlay", () => {
  it("renders the five groups, the leader between BROWSE and Anywhere, the last headed by the configured prefix", () => {
    const { container } = overlay();
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual(["This panel", "Leader and tab keys", "Typing", "Anywhere in the window", "After Ctrl+b"]);
  });

  it("follows a user's prefix", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+a" panel={EMPTY_PANEL_TABLE} />,
    );
    // No `.at(-1)`: tsconfig targets ES2020 (see `lastOfType` in `App.test.tsx`).
    const titles = Array.from(container.querySelectorAll("h2"));
    expect(titles[titles.length - 1].textContent).toBe("After Ctrl+a");
  });

  it("lists every row it was given and every local row, none dropped", () => {
    const { container } = overlay();
    const text = container.textContent!;
    for (const row of [...BROWSE_KEYS, ...INPUT_KEYS, ...WINDOW, ...PREFIX]) {
      expect(text).toContain(row.keys);
      expect(text).toContain(row.what);
    }
    // Typing carries two rows of its own on top of INPUT_KEYS (C6): the pane-switch chord and the
    // prefix, both of which neovibe keeps for itself rather than handing to the composer. The
    // leader section adds none of its own -- `EMPTY_PANEL_TABLE` has no bindings.
    expect(container.querySelectorAll("tr").length).toBe(
      BROWSE_KEYS.length + INPUT_KEYS.length + 2 + WINDOW.length + PREFIX.length,
    );
  });

  it("names the chords neovibe keeps while typing (C6)", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={[]} prefixKeys={[]} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} />,
    );
    const typing = Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Typing"))!;
    expect(typing.textContent).toContain("Ctrl+h / j / k / l");
    expect(typing.textContent).toContain("Move between panes (neovibe keeps these)");
    expect(typing.textContent).toContain("Ctrl+b");
  });

  it("closes on a click on its own backdrop, not on a click inside a table", () => {
    const onClose = vi.fn();
    const { container } = overlay(onClose);
    fireEvent.click(container.querySelector("table")!);
    fireEvent.click(container.querySelector("td")!);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(container.querySelector(".keymap-overlay")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  describe("the leader and tab keys section (panel round 2 plan, Task 8)", () => {
    function leaderSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Leader and tab keys"))!;
    }

    it("names the leader by each source (Review Focus 4)", () => {
      expect(leaderSection(overlay(() => {}, EMPTY_PANEL_TABLE).container).querySelector("p")!.textContent).toBe(
        "leader: Space (default)",
      );
      expect(
        leaderSection(overlay(() => {}, { ...EMPTY_PANEL_TABLE, leaderSource: "mapleader" }).container).querySelector("p")!
          .textContent,
      ).toBe("leader: Space (nvim's mapleader)");
      expect(
        leaderSection(overlay(() => {}, { ...EMPTY_PANEL_TABLE, leaderSource: "unset" }).container).querySelector("p")!
          .textContent,
      ).toBe("leader: Space (mapleader is unset)");
      expect(
        leaderSection(overlay(() => {}, { ...EMPTY_PANEL_TABLE, leaderSource: "unusable" }).container).querySelector("p")!
          .textContent,
      ).toBe("leader: Space (nvim's mapleader is not usable here)");
    });

    it("lists a binding as its full key sequence and description", () => {
      const section = leaderSection(overlay(() => {}, TABLE).container);
      const row = Array.from(section.querySelectorAll("tr")).find((tr) => tr.textContent?.includes("close tab"))!;
      expect(row).toBeDefined();
      expect(row.querySelector(".keycap")!.textContent).toBe("Space b d");
      expect(row.textContent).toContain("close tab");
    });

    it("suffixes a row by its source: nothing for default, (from nvim), (init.lua)", () => {
      const panel: PanelTable = {
        ...TABLE,
        bindings: [
          { keys: ["H"], action: "tab.prev", desc: "prev tab", source: "default" },
          { keys: ["L"], action: "tab.next", desc: "next tab", source: "nvim" },
          { keys: ["<leader>", "q"], action: "tab.close", desc: "quit tab", source: "init.lua" },
        ],
      };
      const text = leaderSection(overlay(() => {}, panel).container).textContent!;
      expect(text).toContain("prev tab");
      expect(text).not.toContain("prev tab (from nvim)");
      expect(text).not.toContain("prev tab (init.lua)");
      expect(text).toContain("next tab (from nvim)");
      expect(text).toContain("quit tab (init.lua)");
    });
  });
});
