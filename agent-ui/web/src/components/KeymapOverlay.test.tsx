// @vitest-environment jsdom
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { KeymapOverlay } from "./KeymapOverlay";
import { BROWSE_KEYS, CARD_KEYS, CARET_KEYS, EMPTY_PANEL_TABLE, INPUT_KEYS, QUESTION_KEYS, REVIEW_KEYS, TASKS_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelTable, PluginKeysHelp } from "../keymap";
import { binding, TABLE } from "../testFixtures";
import { listedSlashCommands } from "../slashCommands";
import { MODE_CONSEQUENCE } from "./Dashboard";

afterEach(cleanup);

const WINDOW: KeyHelp[] = [{ keys: "F11", what: "Fullscreen" }];
const PREFIX: KeyHelp[] = [
  { keys: "Ctrl+b f", what: "HINT: jump anywhere in the window" },
  { keys: "Ctrl+b %", what: "Then a module key (e / a / t): open it right of this one, or move it there" },
];

function overlay(onClose = () => {}, panel: PanelTable = EMPTY_PANEL_TABLE) {
  return render(<KeymapOverlay onClose={onClose} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={panel} />);
}

/** A section by its own `<h2>`, so a test about one group's note cannot be satisfied by another's. */
function sectionTitled(container: HTMLElement, title: string) {
  return Array.from(container.querySelectorAll("section")).find((s) => s.querySelector("h2")?.textContent === title)!;
}

describe("KeymapOverlay", () => {
  it.each(["Ctrl+b", "Ctrl+a"])("names the sheet's scroll and close keys under %s", (prefix) => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel={prefix} panel={EMPTY_PANEL_TABLE} />,
    );
    const help = Array.from(container.querySelectorAll("p")).find((p) => /scroll/i.test(p.textContent ?? ""));
    expect(help).toBeDefined();
    for (const key of ["j/k", "↓/↑", "Ctrl+d/u", "Ctrl+f", "PageDown/PageUp", "Space", "gg/G", "?", "Esc", "q"]) {
      expect(help!.textContent).toContain(key);
    }
    if (prefix === "Ctrl+b") {
      expect(help!.textContent).toContain("Ctrl+b is the prefix");
      expect(help!.textContent).not.toContain("Ctrl+f/b");
    } else {
      expect(help!.textContent).toContain("Ctrl+f/b");
      expect(help!.textContent).not.toContain("Ctrl+b is the prefix");
    }
    expect(help!.textContent).toMatch(/close/i);
  });

  it("renders its groups, the leader between BROWSE and Typing, Slash commands then the tasks list and question keys between Typing and Anywhere, the last headed by the configured prefix", () => {
    const { container } = overlay();
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual([
      "This panel",
      "Answering a card",
      "Selecting (v)",
      "Review (c)",
      "Leader and tab keys",
      "Typing",
      "Slash commands",
      "Background tasks (/tasks)",
      "Claude's question",
      "Anywhere in the window",
      "After Ctrl+b",
    ]);
  });

  it("names the connected phones first, with their grants and hosts, and no section without any", () => {
    const phones = [
      { device: "d1", name: "Pixel\u202e", grant: "watch" as const, host: "127.0.0.1:7341", since: 5 },
      { device: "d2", name: "Tablet", grant: "watch+steer" as const, host: "desk.example.org", since: 9 },
    ];
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} phones={phones} />,
    );
    expect(container.querySelector("h2")?.textContent).toBe("Phones connected");
    const text = sectionTitled(container, "Phones connected").textContent ?? "";
    expect(text).toContain("Pixel⟨U+202E⟩");
    expect(text).toContain("watch");
    expect(text).toContain("127.0.0.1:7341");
    expect(text).toContain("Tablet");
    expect(text).toContain("watch+steer");
    expect(text).toContain("desk.example.org");
    for (const none of [undefined, []]) {
      const bare = render(
        <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} phones={none} />,
      );
      expect(Array.from(bare.container.querySelectorAll("h2")).map((h) => h.textContent)).not.toContain("Phones connected");
      bare.unmount();
    }
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
    for (const row of [...BROWSE_KEYS, ...CARD_KEYS, ...CARET_KEYS, ...VISUAL_KEYS, ...REVIEW_KEYS, ...INPUT_KEYS, ...TASKS_KEYS, ...QUESTION_KEYS, ...WINDOW, ...PREFIX]) {
      expect(text).toContain(row.keys);
      expect(text).toContain(row.what);
    }
    // Typing carries four rows of its own on top of INPUT_KEYS: the pane-switch chord and the
    // prefix, plus the mirror's `Ctrl+k`/`Ctrl+j`; all four Eitri (or Rust's mirror)
    // keeps for itself rather than handing to the composer. "This panel" carries one of its own on
    // top of BROWSE_KEYS: `Ctrl+j`, Rust's mirror claiming the chord ahead of `resolveKey`.
    // Neither GTK-decided pair can live in `BROWSE_KEYS`/`INPUT_KEYS` themselves -- both are tied to
    // `resolveKey`/`COMPOSER_CHORDS` both ways (`keymap.test.ts`, `composerKeys.test.ts`). The leader
    // section adds none of its own; `EMPTY_PANEL_TABLE` has no bindings. "Selecting"
    // adds `CARET_KEYS.length + VISUAL_KEYS.length` rows of its own with no
    // GTK-decided extra, since the region never reaches `shell`. "Review (c)" adds `REVIEW_KEYS.length`, and the
    // Claude tab's tasks list and question sections add theirs.
    expect(container.querySelectorAll("tr").length).toBe(
      BROWSE_KEYS.length + 1 + CARD_KEYS.length + CARET_KEYS.length + VISUAL_KEYS.length + REVIEW_KEYS.length + INPUT_KEYS.length + 4 +
        TASKS_KEYS.length + QUESTION_KEYS.length + WINDOW.length + PREFIX.length,
    );
  });

  it("names the chords Eitri keeps while typing", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={[]} prefixKeys={[]} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} />,
    );
    const typing = Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Typing"))!;
    // Narrowed to h/l -- j/k are claimed by the two rows below
    // instead, so the pane row no longer claims them too.
    expect(typing.textContent).toContain("Ctrl+h / l");
    expect(typing.textContent).toContain("Move between panes (Eitri keeps these)");
    expect(typing.textContent).toContain("Ctrl+b");
    // The mirror's own Ctrl+k/Ctrl+j, spliced in the same way.
    expect(typing.textContent).toContain("Back to browsing (as Esc)");
    expect(typing.textContent).toContain("The module below");
  });

  // The mirror's own `Ctrl+j` in "This panel" is spliced in the same way, since
  // `resolveKey` never claims it either -- checked against a real panel `<Section>`, not just the
  // whole-container row count above.
  it("names the mirror's Ctrl+j in This panel too", () => {
    const { container } = overlay();
    const panelSection = Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("This panel"))!;
    expect(panelSection.textContent).toContain("Ctrl+j");
    expect(panelSection.textContent).toContain("Type (the box below)");
  });

  // `Ctrl+[` is Esc is one listener on the document (`../ctrlBracket`), never a
  // key `resolveKey` sees, so it cannot be a row of `BROWSE_KEYS` -- `keymap.test.ts` ties that table
  // to `resolveKey` both ways and would fail on it. It is a note under the table instead, and a section
  // without a note draws no `<p>` at all.
  describe("the note under This panel", () => {
    it("says on another agent's tab that manual asks for what reaches Eitri, and where the rest is described", () => {
      const { container } = render(
        <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+a" panel={EMPTY_PANEL_TABLE} agent="Codex" />,
      );
      expect(sectionTitled(container, "This panel").querySelector("p")!.textContent).toContain(
        "manual — Every request Codex makes asks; what it runs without asking is in Ctrl+a i.",
      );
    });

    it("says on a Claude tab that manual asks for every edit, and on the tab's own cycle row", () => {
      const { container } = overlay();
      expect(sectionTitled(container, "This panel").querySelector("p")!.textContent).toContain(
        "manual — Safe reads run; every edit and anything else asks. auto —",
      );
      expect(sectionTitled(container, "Typing").textContent).toContain(
        "Cycle manual → auto → bypass (entering bypass asks first)",
      );
    });

    it("names only the modes the tab offers: an ACP agent's tab has no manual, in the note or the Shift+Tab row", () => {
      const { container } = render(
        <KeymapOverlay
          onClose={() => {}}
          windowKeys={WINDOW}
          prefixKeys={PREFIX}
          prefixLabel="Ctrl+b"
          panel={EMPTY_PANEL_TABLE}
          agent="gemini"
          modes={["auto", "bypass"]}
        />,
      );
      expect(sectionTitled(container, "This panel").querySelector("p")!.textContent).toBe(
        `Ctrl+[ is Esc everywhere in this panel. Modes (Shift+Tab): auto — ${MODE_CONSEQUENCE.auto} bypass — ${MODE_CONSEQUENCE.bypass}`,
      );
      const typing = sectionTitled(container, "Typing").textContent!;
      expect(typing).toContain("Cycle auto → bypass (entering bypass asks first)");
      expect(container.textContent).not.toContain("manual");
    });

    it("says Ctrl+[ is Esc and what each mode does, in a <p> after the table, and adds no row", () => {
      const section = sectionTitled(overlay().container, "This panel");
      expect(section.querySelectorAll("p")).toHaveLength(1);
      const note = section.querySelector("p")!;
      // Consequence lines follow Shift+Tab's mode order.
      expect(note.textContent).toBe(
        `Ctrl+[ is Esc everywhere in this panel. Modes (Shift+Tab): manual — ${MODE_CONSEQUENCE.manual} auto — ${MODE_CONSEQUENCE.auto} bypass — ${MODE_CONSEQUENCE.bypass}`,
      );
      expect(section.querySelector("table")!.nextElementSibling).toBe(note);
      // BROWSE_KEYS, plus the mirror's own `Ctrl+j` -- the same count the whole-container test pins.
      expect(section.querySelectorAll("tr")).toHaveLength(BROWSE_KEYS.length + 1);
    });

    it("draws no <p> in a section that gives no note", () => {
      const { container } = overlay();
      expect(sectionTitled(container, "Anywhere in the window").querySelector("p")).toBeNull();
      expect(sectionTitled(container, "After Ctrl+b").querySelector("p")).toBeNull();
    });
  });

  // What rides with a sent message is one sentence about
  // Enter, not a key of its own -- `Composer` sends it and `resolveKey` never sees it, so it cannot be a
  // row of `INPUT_KEYS` (tied to `COMPOSER_CHORDS` both ways) -- and it is the note under "Typing", the
  // same shape as the Escape-alias note under "This panel". The figures are the nvim snippet's own caps
  // (`MAX_SELECTION_LINES = 400`, `CONTENT_LIMIT = 2000`); "not its text or cursor" is because
  // `core/src/editor_context/feed.rs` drops the cursor line and the file's text is never sent.
  describe("the note under Typing", () => {
    it("says what Enter also sends from the editor, in a <p> after the table, and adds no row", () => {
      const section = sectionTitled(overlay().container, "Typing");
      expect(section.querySelectorAll("p")).toHaveLength(1);
      const note = section.querySelector("p")!;
      expect(note.textContent).toBe(
        "Enter also sends the editor's file name — not its text or cursor — or the lines selected there in Visual mode (up to 400 lines and 2000 characters). /model, /effort and the CLI's other local commands go without it.",
      );
      expect(section.querySelector("table")!.nextElementSibling).toBe(note);
      // INPUT_KEYS, plus the four rows this section splices in (the pane chord, the prefix, `Ctrl+k`,
      // `Ctrl+j`) -- the same count the whole-container test pins.
      expect(section.querySelectorAll("tr")).toHaveLength(INPUT_KEYS.length + 4);
    });
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

  describe("the slash commands section", () => {
    function slashSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Slash commands"))!;
    }

    // A bare /model sends too now, so it is listed plainly.
    it("lists the works rows Enter would send, in the table's own order -- no /config, and /model plain", () => {
      const section = slashSection(overlay().container);
      const items = Array.from(section.querySelectorAll("li")).map((li) => li.textContent);
      expect(items).toEqual(listedSlashCommands().map((name) => `/${name}`));
      expect(items).not.toContain("/config");
      expect(items).toContain("/model");
    });

    it("is left out in another agent's tab: its slash commands are Claude Code's", () => {
      const { container } = render(
        <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} claudeSlashCommands={false} />,
      );
      const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
      expect(titles).not.toContain("Slash commands");
      expect(titles).not.toContain("Background tasks (/tasks)");
      expect(titles).not.toContain("Claude's question");
      expect(titles).toContain("Typing");
    });

    it("adds no <tr> of its own -- it is a plain list, not a key table", () => {
      const section = slashSection(overlay().container);
      expect(section.querySelectorAll("tr").length).toBe(0);
      expect(section.querySelectorAll("table").length).toBe(0);
    });
  });

  describe("the leader and tab keys section", () => {
    function leaderSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.textContent?.startsWith("Leader and tab keys"))!;
    }

    it("names the leader by each source", () => {
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

  describe("Selecting (v)", () => {
    function selectingSection(container: HTMLElement) {
      return Array.from(container.querySelectorAll("section")).find((s) => s.querySelector("h2")?.textContent === "Selecting (v)")!;
    }

    it("lists every CARET_KEYS and VISUAL_KEYS row, and the default note when nothing claims v or V", () => {
      const section = selectingSection(overlay(() => {}, TABLE).container);
      for (const row of [...CARET_KEYS, ...VISUAL_KEYS]) {
        expect(section.textContent).toContain(row.keys);
        expect(section.textContent).toContain(row.what);
      }
      // Ctrl+e/Ctrl+y now scroll the region instead of ending
      // it, so "any other key leaves" is no longer literally true -- the note names the exception.
      expect(section.textContent).toContain(
        "Any other key leaves (Ctrl+e/Ctrl+y scroll instead). The panel is read-only; Ctrl+g opens the row in nvim for search, text objects, registers.",
      );
    });

    // Each mode needs its own headed table: shared keys such as gg and Esc otherwise read as
    // duplicates with conflicting meanings. Compare each table to its source array, in order;
    // whole-section text checks cannot distinguish shared descriptions between CARET and VISUAL.
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

    // A plain binding on v is not a leader; the note must name which one actually claims the key.
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

/* The PageUp row said "(Ctrl+b is the prefix)" whatever the prefix was: under a Ctrl+a prefix it
   named the wrong key. It now follows the effective prefix (`prefixLabel`, the same source as the
   "After <prefix>" heading): the parenthesis says why vim's Ctrl+b is not the way back a page only when
   Ctrl+b IS the prefix. */
describe("KeymapOverlay: the lines of the user's tmux config the import skipped", () => {
  const SKIPPED: KeyHelp[] = [
    { keys: "~/.config/tmux/base.conf:82", what: "bind P paste-buffer \u2014 no Eitri equivalent: paste-buffer" },
    { keys: "~/.config/tmux/tmux.conf:11", what: "run '~/.config/tmux/plugins/tpm/tpm' \u2014 run-shell runs a shell command, which Eitri never does" },
  ];

  it("lists each skipped line last, under its own heading, with where it is and why", () => {
    const { container } = render(
      <KeymapOverlay
        onClose={() => {}}
        windowKeys={WINDOW}
        prefixKeys={PREFIX}
        prefixLabel="Ctrl+a"
        panel={EMPTY_PANEL_TABLE}
        tmuxSkipped={SKIPPED}
      />,
    );
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles.slice(-2)).toEqual(["After Ctrl+a", "Skipped from tmux"]);
    const section = sectionTitled(container, "Skipped from tmux");
    for (const row of SKIPPED) {
      expect(section.textContent).toContain(row.keys);
      expect(section.textContent).toContain(row.what);
    }
  });

  it("draws no such section when nothing was skipped or the import is off", () => {
    for (const tmuxSkipped of [[], undefined]) {
      const { container } = render(
        <KeymapOverlay
          onClose={() => {}}
          windowKeys={WINDOW}
          prefixKeys={PREFIX}
          prefixLabel="Ctrl+b"
          panel={EMPTY_PANEL_TABLE}
          tmuxSkipped={tmuxSkipped}
        />,
      );
      expect(sectionTitled(container, "Skipped from tmux")).toBeUndefined();
      cleanup();
    }
  });
});

describe("KeymapOverlay: the PageUp row follows the configured prefix", () => {
  const pageUpRow = (container: HTMLElement) =>
    Array.from(sectionTitled(container, "This panel").querySelectorAll("tr, li, div")).find(
      (el) => el.children.length >= 2 && el.children[0].textContent === "PageUp",
    )!;
  it("under Ctrl+a, the row names no Ctrl+b", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+a" panel={EMPTY_PANEL_TABLE} />,
    );
    const row = pageUpRow(container);
    expect(row).toBeDefined();
    expect(row.textContent).toContain("A view up");
    expect(row.textContent).not.toContain("Ctrl+b");
  });
  it("under the stock Ctrl+b, the row says Ctrl+b is the prefix", () => {
    const { container } = overlay();
    expect(pageUpRow(container).textContent).toContain("A view up (Ctrl+b is the prefix)");
  });
});

describe("KeymapOverlay: a companion window", () => {
  const NOTE = "A companion window has no layout keys: your window manager arranges windows.";
  const COMPANION_ROOT: KeyHelp[] = [{ keys: "Ctrl+h / Ctrl+l", what: "leave the panel (your window manager moves focus)" }];
  const COMPANION_PREFIX: KeyHelp[] = [{ keys: "Ctrl+b c", what: "New session tab" }];

  it("companion note shown and module rows absent", () => {
    const { container } = render(
      <KeymapOverlay
        onClose={() => {}}
        windowKeys={COMPANION_ROOT}
        prefixKeys={COMPANION_PREFIX}
        prefixLabel="Ctrl+b"
        panel={EMPTY_PANEL_TABLE}
        companion
      />,
    );
    const prefix = sectionTitled(container, "After Ctrl+b");
    expect(prefix.querySelector("p")!.textContent).toBe(NOTE);
    expect(prefix.textContent).toContain("New session tab");
    // Only what the shell sent is listed: no module, split or layout row is added here.
    expect(prefix.textContent).not.toContain("module");
    expect(sectionTitled(container, "Anywhere in the window").textContent).toContain("leave the panel");
  });

  it("says nothing about layout keys in the one-window mode", () => {
    const { container } = overlay();
    expect(container.textContent).not.toContain(NOTE);
    expect(sectionTitled(container, "After Ctrl+b").querySelector("p")).toBeNull();
  });
});

describe("KeymapOverlay: a plugin page's section", () => {
  function withPlugin(plugin: PluginKeysHelp | undefined) {
    return render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" panel={EMPTY_PANEL_TABLE} plugin={plugin} />,
    );
  }

  it("comes first, under the plugin's title, with its rows", () => {
    const { container } = withPlugin({
      title: "Snotra",
      declared: true,
      rows: [
        { keys: "]q / <C-n>", what: "题目 · 下一题" },
        { keys: "?", what: "按键帮助" },
      ],
    });
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles[0]).toBe("Snotra");
    expect(titles[1]).toBe("This panel");
    const section = sectionTitled(container, "Snotra");
    expect(section.querySelectorAll("tr")).toHaveLength(2);
    expect(section.querySelector("kbd")!.textContent).toBe("]q / <C-n>");
    expect(section.textContent).toContain("题目 · 下一题");
  });

  it("renders a row's text as text, never as markup", () => {
    const { container } = withPlugin({
      title: "<b>Snotra</b>",
      declared: true,
      rows: [{ keys: "<img src=x>", what: "<img src=x onerror=alert(1)>" }],
    });
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("b")).toBeNull();
    const section = sectionTitled(container, "<b>Snotra</b>");
    expect(section.querySelector("kbd")!.textContent).toBe("<img src=x>");
    expect(section.querySelectorAll("td")[1].textContent).toBe("<img src=x onerror=alert(1)>");
  });

  it("renders a folded generic row's context as text", () => {
    const what = '<img src=x onerror="alert(1)">';
    const { container } = withPlugin({
      title: "Plugin",
      declared: true,
      rows: [{ keys: "j / <C-e> k f", what }],
    });
    const section = sectionTitled(container, "Plugin");
    expect(section.querySelector("img")).toBeNull();
    expect(section.querySelectorAll("td")[1].textContent).toBe(what);
  });

  it("says so when the page has declared no keys", () => {
    const { container } = withPlugin({ title: "Snotra", declared: false, rows: [] });
    const section = sectionTitled(container, "Snotra");
    expect(section.querySelectorAll("tr")).toHaveLength(0);
    expect(section.textContent).toContain("This plugin has not declared its keys");
  });

  it("draws an empty section, without the undeclared note, for a page that declared and kept no key", () => {
    const { container } = withPlugin({ title: "Snotra", declared: true, rows: [] });
    const section = sectionTitled(container, "Snotra");
    expect(section.querySelectorAll("tr")).toHaveLength(0);
    expect(section.textContent).not.toContain("This plugin has not declared its keys");
  });

  it("is not drawn without a plugin", () => {
    const { container } = withPlugin(undefined);
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles[0]).toBe("This panel");
    expect(container.textContent).not.toContain("This plugin has not declared its keys");
  });
});

describe("KeymapOverlay opening position", () => {
  const original = Object.getOwnPropertyDescriptor(Element.prototype, "scrollIntoView");
  const scrollIntoView = vi.fn();

  beforeEach(() => {
    scrollIntoView.mockClear();
    Object.defineProperty(Element.prototype, "scrollIntoView", { configurable: true, value: scrollIntoView });
  });

  afterEach(() => {
    if (original) Object.defineProperty(Element.prototype, "scrollIntoView", original);
    else Reflect.deleteProperty(Element.prototype, "scrollIntoView");
  });

  it("prefix_question_opens_at_the_prefix_table", () => {
    const props = { onClose: () => {}, windowKeys: WINDOW, prefixKeys: PREFIX, prefixLabel: "Ctrl+b", panel: EMPTY_PANEL_TABLE };
    const { container, rerender } = render(<KeymapOverlay {...props} startAt="prefix" />, { wrapper: StrictMode });
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    expect(scrollIntoView.mock.instances[0]).toBe(sectionTitled(container, "After Ctrl+b").querySelector("h2"));
    expect(scrollIntoView).toHaveBeenCalledWith({ block: "start" });
    rerender(<KeymapOverlay {...props} startAt="prefix" />);
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
  });

  it("a_prefix_request_over_an_open_sheet_moves_it_to_the_prefix_table", () => {
    const props = { onClose: () => {}, windowKeys: WINDOW, prefixKeys: PREFIX, prefixLabel: "Ctrl+b", panel: EMPTY_PANEL_TABLE };
    const { container, rerender } = render(<KeymapOverlay {...props} startRequest={0} />, { wrapper: StrictMode });
    expect(scrollIntoView).not.toHaveBeenCalled();
    rerender(<KeymapOverlay {...props} startAt="prefix" startRequest={1} />);
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    expect(scrollIntoView.mock.instances[0]).toBe(sectionTitled(container, "After Ctrl+b").querySelector("h2"));
    rerender(<KeymapOverlay {...props} startAt="prefix" startRequest={1} />);
    expect(scrollIntoView, "a re-render of the same request leaves the scroll alone").toHaveBeenCalledTimes(1);
  });

  it("F1 in card mode opens at the card-mode keys", () => {
    const props = { onClose: () => {}, windowKeys: WINDOW, prefixKeys: PREFIX, prefixLabel: "Ctrl+b", panel: EMPTY_PANEL_TABLE };
    const { container } = render(<KeymapOverlay {...props} openAt="card" startRequest={1} />, { wrapper: StrictMode });
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    expect(scrollIntoView.mock.instances[0]).toBe(sectionTitled(container, "Answering a card").querySelector("h2"));
    expect(sectionTitled(container, "Answering a card").querySelectorAll("tr")).toHaveLength(CARD_KEYS.length);
  });

  it.each([
    ["browse", "This panel"], ["input", "Typing"], ["selecting", "Selecting (v)"], ["question", "Claude's question"], ["card", "Answering a card"],
  ] as const)("openAt=%s positions its heading once per request", (openAt, title) => {
    const props = { onClose: () => {}, windowKeys: WINDOW, prefixKeys: PREFIX, prefixLabel: "Ctrl+b", panel: EMPTY_PANEL_TABLE };
    const { container, rerender } = render(<KeymapOverlay {...props} openAt={openAt} startRequest={1} />, { wrapper: StrictMode });
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    expect(scrollIntoView.mock.instances[0]).toBe(sectionTitled(container, title).querySelector("h2"));
    rerender(<KeymapOverlay {...props} openAt={openAt} startRequest={1} />);
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    rerender(<KeymapOverlay {...props} openAt={openAt} startRequest={2} />);
    expect(scrollIntoView).toHaveBeenCalledTimes(2);
  });

  it("the_chats_own_question_opens_on_this_panel", () => {
    const { container } = overlay();
    expect(scrollIntoView).not.toHaveBeenCalled();
    expect(container.querySelector("h2")?.textContent).toBe("This panel");
  });

  it("a_pages_rows_stay_first", () => {
    const { container } = render(
      <KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b"
        panel={EMPTY_PANEL_TABLE} startAt="prefix" plugin={{ title: "Page keys", rows: [], declared: true }} />,
    );
    expect(scrollIntoView).not.toHaveBeenCalled();
    expect(container.querySelector("h2")?.textContent).toBe("Page keys");
  });
});

it("lists Pages with their load verb as text, without controls, and hides an empty section", () => {
  const props = { onClose: vi.fn(), windowKeys: WINDOW, prefixKeys: PREFIX, prefixLabel: "Ctrl+b", panel: EMPTY_PANEL_TABLE };
  const rendered = render(<KeymapOverlay {...props} pages={[{ id: "markdown", title: "<b>Markdown</b>", summary: null }]} />);
  const section = sectionTitled(rendered.container, "Pages");
  expect(section).toBeDefined();
  expect(section.textContent).toContain("<b>Markdown</b>");
  expect(section.textContent).toContain(":Eitri plugin load markdown");
  expect(section.querySelector("b, button, [tabindex], [aria-current]")).toBeNull();
  rendered.rerender(<KeymapOverlay {...props} pages={[]} />);
  expect(sectionTitled(rendered.container, "Pages")).toBeUndefined();
});

it("shows each motion's effective keys, dropping keys taken by another action", () => {
  const panel: PanelTable = { ...TABLE, bindings: [
    ...TABLE.bindings, binding(["j"], "line.down"), binding(["x"], "row.down"),
    binding(["}"], "row.down"), binding(["]", "t"], "tool.prev"), binding(["G"], "top"),
    binding(["n"], "help"), binding(["g", "g"], "top"), binding(["f"], "row.down"), binding(["h"], "row.up"),
  ] };
  const { container } = overlay(() => {}, panel);
  const rows = Array.from(sectionTitled(container, "This panel").querySelectorAll("tr"));
  const keys = (what: string) => rows.find((row) => row.querySelectorAll("td")[1].textContent === what)?.querySelector("kbd")?.textContent;
  expect(keys("Next row; a long row scrolls first; alone on the last row goes to the box")).toBe("↓ / x / } / f");
  expect(keys("One line down (a count repeats it, e.g. 5 Ctrl+e)")).toBe("Ctrl+e / j");
  expect(keys("Previous tool call")).toBe("[t / ]t");
  expect(keys("Next tool call")).toBe("Unbound");
  expect(keys("First row")).toBe("gg / G");
  expect(keys("Last row")).toBe("Unbound");
  expect(keys("This list")).toBe("F1 / g? / n");
  expect(keys("HINT: jump anywhere in the window (links too; Enter opens one)")).toBe("f");
  expect(keys("Previous / next button in the row")).toBe("h / l");
  expect(keys("Next match, wrapping")).toBe("Unbound");
  expect(sectionTitled(container, "Leader and tab keys").textContent).not.toContain("line.down");
});
