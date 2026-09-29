// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { KeymapOverlay } from "./KeymapOverlay";
import { BROWSE_KEYS, CARET_KEYS, EMPTY_PANEL_TABLE, INPUT_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelTable } from "../keymap";
import { binding, TABLE } from "../testFixtures";
import { listedSlashCommands } from "../slashCommands";

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
  it("renders the six groups, the leader between BROWSE and Typing, Slash commands between Typing and Anywhere, the last headed by the configured prefix", () => {
    const { container } = overlay();
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual([
      "This panel",
      "Selecting (v)",
      "Leader and tab keys",
      "Typing",
      "Slash commands",
      "Anywhere in the window",
      "After Ctrl+b",
    ]);
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
    for (const row of [...BROWSE_KEYS, ...CARET_KEYS, ...VISUAL_KEYS, ...INPUT_KEYS, ...WINDOW, ...PREFIX]) {
      expect(text).toContain(row.keys);
      expect(text).toContain(row.what);
    }
    // Typing carries four rows of its own on top of INPUT_KEYS: the pane-switch chord and the
    // prefix (C6), plus V1 C1's `Ctrl+k`/`Ctrl+j` -- all four of which neovibe (or Rust's mirror)
    // keeps for itself rather than handing to the composer. "This panel" carries one of its own on
    // top of BROWSE_KEYS (V1 C1): `Ctrl+j`, Rust's mirror claiming the chord ahead of `resolveKey`.
    // Neither GTK-decided pair can live in `BROWSE_KEYS`/`INPUT_KEYS` themselves -- both are tied to
    // `resolveKey`/`COMPOSER_CHORDS` both ways (`keymap.test.ts`, `composerKeys.test.ts`). The leader
    // section adds none of its own -- `EMPTY_PANEL_TABLE` has no bindings. "Selecting" (visual-mode
    // spec, revised for 3a) adds `CARET_KEYS.length + VISUAL_KEYS.length` rows of its own -- no
    // GTK-decided extra, since the region never reaches `shell`.
    expect(container.querySelectorAll("tr").length).toBe(
      BROWSE_KEYS.length + 1 + CARET_KEYS.length + VISUAL_KEYS.length + INPUT_KEYS.length + 4 + WINDOW.length + PREFIX.length,
    );
  });

  it("names the chords neovibe keeps while typing (C6)", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={[]} prefixKeys={[]} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} />,
    );
    const typing = Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Typing"))!;
    // Fix round 1 (reviewer finding): narrowed to h/l -- j/k are claimed by the two rows below
    // instead, so the pane row no longer claims them too.
    expect(typing.textContent).toContain("Ctrl+h / l");
    expect(typing.textContent).toContain("Move between panes (neovibe keeps these)");
    expect(typing.textContent).toContain("Ctrl+b");
    // V1 C1 (spec §3.1, §3.5): the mirror's own Ctrl+k/Ctrl+j, spliced in the same way.
    expect(typing.textContent).toContain("Back to browsing (as Esc)");
    expect(typing.textContent).toContain("The module below");
  });

  // V1 C1 (spec §3.1): the mirror's own `Ctrl+j` in "This panel" is spliced in the same way, since
  // `resolveKey` never claims it either -- checked against a real panel `<Section>`, not just the
  // whole-container row count above.
  it("names the mirror's Ctrl+j in This panel too", () => {
    const { container } = overlay();
    const panelSection = Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("This panel"))!;
    expect(panelSection.textContent).toContain("Ctrl+j");
    expect(panelSection.textContent).toContain("Type (the box below)");
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

  describe("the slash commands section (spec §9.2, P10)", () => {
    function slashSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Slash commands"))!;
    }

    // Owner trial item 2 (2026-09-28): a bare /model sends too now, so it is listed plainly.
    it("lists the works rows Enter would send, in the table's own order -- no /config, and /model plain", () => {
      const section = slashSection(overlay().container);
      const items = Array.from(section.querySelectorAll("li")).map((li) => li.textContent);
      expect(items).toEqual(listedSlashCommands().map((name) => `/${name}`));
      expect(items).not.toContain("/config");
      expect(items).toContain("/model");
    });

    it("adds no <tr> of its own -- it is a plain list, not a key table", () => {
      const section = slashSection(overlay().container);
      expect(section.querySelectorAll("tr").length).toBe(0);
      expect(section.querySelectorAll("table").length).toBe(0);
    });
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

  describe("Selecting (v) -- visual-mode spec D2, revised for 3a", () => {
    function selectingSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.querySelector("h2")?.textContent === "Selecting (v)")!;
    }

    it("lists every CARET_KEYS and VISUAL_KEYS row, and the default note when nothing claims v or V", () => {
      const section = selectingSection(overlay(() => {}, TABLE).container);
      for (const row of [...CARET_KEYS, ...VISUAL_KEYS]) {
        expect(section.textContent).toContain(row.keys);
        expect(section.textContent).toContain(row.what);
      }
      // Seam review finding 3 (2026-09-28): Ctrl+e/Ctrl+y now scroll the region instead of ending
      // it, so "any other key leaves" is no longer literally true -- the note names the exception.
      expect(section.textContent).toContain(
        "Any other key leaves (Ctrl+e/Ctrl+y scroll instead). The panel is read-only; Ctrl+g opens the row in nvim for search, text objects, registers.",
      );
    });

    // Fix round 2 (reviewer finding, minor): CARET_KEYS and VISUAL_KEYS used to render as one table
    // with no sub-heading -- 21 rows, duplicates (a key both modes bind, e.g. `gg`) indistinguishable,
    // and two conflicting `Esc` rows with nothing saying which mode either belongs to. Assert the
    // grouping itself -- each table's own rows, in order, exactly match its own source array -- not
    // just that every row's text is present somewhere in the section (`toContain` alone cannot tell
    // CARET's rows apart from VISUAL's, since several `what` strings are shared verbatim between the
    // two, e.g. "Previous / next character").
    it("renders CARET's table then VISUAL's, each under its own sub-heading, never merged", () => {
      const section = selectingSection(overlay(() => {}, TABLE).container);
      const headings = Array.from(section.querySelectorAll("h3")).map((h) => h.textContent);
      expect(headings).toEqual(["CARET", "VISUAL"]);
      const tables = Array.from(section.querySelectorAll("table"));
      expect(tables).toHaveLength(2);
      const [caretTable, visualTable] = tables;
      const rowTexts = (table: Element) => Array.from(table.querySelectorAll("tr")).map((tr) => tr.textContent);
      expect(rowTexts(caretTable)).toEqual(CARET_KEYS.map((row) => `${row.keys}${row.what}`));
      expect(rowTexts(visualTable)).toEqual(VISUAL_KEYS.map((row) => `${row.keys}${row.what}`));
      // CARET's own two rows with no VISUAL equivalent stay only in CARET's table.
      expect(caretTable.textContent).toContain("Back to BROWSE, cursor on this row");
      expect(visualTable.textContent).not.toContain("Back to BROWSE, cursor on this row");
      // VISUAL's own rows (copy/quote/mode-switch) stay only in VISUAL's table.
      expect(visualTable.textContent).toContain("Copy the highlighted text and return to BROWSE");
      expect(caretTable.textContent).not.toContain("Copy the highlighted text and return to BROWSE");
    });

    it("says v is the leader, and how the region is still reached, when the leader is v", () => {
      const panel: PanelTable = { ...TABLE, leader: "v", leaderLabel: "v" };
      const section = selectingSection(overlay(() => {}, panel).container);
      expect(section.textContent).toContain("v is your leader: V selects lines, Esc there gives the caret");
    });

    // Fix round 2 (reviewer finding, minor): a plain panel binding on `v` is not a leader -- the
    // note used to say "v is your leader" for it anyway, naming a leader the user does not have.
    it("says v is bound elsewhere (not that it is the leader) when a plain binding takes v", () => {
      const panel: PanelTable = { ...TABLE, bindings: [binding(["v"], "tab.prev")] };
      const section = selectingSection(overlay(() => {}, panel).container);
      expect(section.textContent).toContain("v is bound elsewhere in this panel: V selects lines, Esc there gives the caret");
      expect(section.textContent).not.toContain("leader");
    });

    it("says V is the leader, and how the caret is still reached, when the leader is V", () => {
      const panel: PanelTable = { ...TABLE, leader: "V", leaderLabel: "V" };
      const section = selectingSection(overlay(() => {}, panel).container);
      expect(section.textContent).toContain("V is your leader: v reaches the caret, then V selects lines");
    });

    it("says V is bound elsewhere when a plain binding takes V", () => {
      const panel: PanelTable = { ...TABLE, bindings: [binding(["V"], "tab.next")] };
      const section = selectingSection(overlay(() => {}, panel).container);
      expect(section.textContent).toContain("V is bound elsewhere in this panel: v reaches the caret, then V selects lines");
    });

    it("says nothing reaches CARET/VISUAL when both v and V are bound elsewhere", () => {
      const panel: PanelTable = { ...TABLE, bindings: [binding(["v"], "tab.prev"), binding(["V"], "tab.next")] };
      const section = selectingSection(overlay(() => {}, panel).container);
      expect(section.textContent).toContain("v and V are both bound elsewhere in this panel, so nothing here reaches CARET or VISUAL.");
    });
  });
});
