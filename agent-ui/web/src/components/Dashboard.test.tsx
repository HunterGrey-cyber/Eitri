// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { DASH_ITEM_KEY, Dashboard, MODE_CONSEQUENCE, dashItems, lastOpenedStamp, modeConsequence } from "./Dashboard";
import type { DashItem } from "./Dashboard";
import type { EditorLink, Hello, PermissionModeChoice, ResumableSession } from "../types";

afterEach(cleanup);

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/Documents/eitri",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: "work",
};

function session(over: Partial<ResumableSession> = {}): ResumableSession {
  return {
    provider: "claude",
    providerSessionId: "id-0000000000",
    createdAt: "1",
    updatedAt: "2",
    title: "zsh 补全 compinit slow",
    name: null,
    ...over,
  };
}

function renderDash(
  over: { hello?: Hello; mode?: PermissionModeChoice; cursor?: number; narrow?: boolean; onItem?: (i: DashItem) => void; editorLink?: EditorLink | null; agent?: string | null } = {},
) {
  const onItem = over.onItem ?? vi.fn();
  const props = {
    hello: over.hello ?? HELLO,
    mode: over.mode ?? "auto",
    cursor: over.cursor ?? 0,
    narrow: over.narrow ?? false,
    onItem,
    editorLink: over.editorLink ?? null,
    agent: over.agent ?? null,
  };
  return { onItem, ...render(<Dashboard {...props} />) };
}

describe("dashItems", () => {
  it("omits resume with no records", () => {
    expect(dashItems(HELLO)).toEqual(["new", "sessions", "keys"]);
  });
  it("offers resume once a record exists", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    expect(dashItems(withRecord)).toEqual(["new", "resume", "sessions", "keys"]);
  });
});

describe("dashItems: restoring the last window's tabs", () => {
  const OFFER = { labels: ["api", "docs"], bypass: 0 };
  it("offers restore right after new, and only with something to bring back", () => {
    expect(dashItems({ ...HELLO, restore: OFFER })).toEqual(["new", "restore", "sessions", "keys"]);
    expect(dashItems({ ...HELLO, resumableSessions: [session()], restore: OFFER })).toEqual([
      "new", "restore", "resume", "sessions", "keys",
    ]);
  });
  it("never draws a dead key: no offer, a null one or an empty one shows nothing", () => {
    expect(dashItems(HELLO)).not.toContain("restore");
    expect(dashItems({ ...HELLO, restore: null })).not.toContain("restore");
    expect(dashItems({ ...HELLO, restore: { labels: [], bypass: 0 } })).not.toContain("restore");
  });
  it("its key is s", () => {
    expect(DASH_ITEM_KEY.restore).toBe("s");
  });
});

describe("Dashboard: the restore line", () => {
  const withOffer = (labels: string[]): Hello => ({ ...HELLO, restore: { labels, bypass: 0 } });
  const restoreRow = (container: HTMLElement) => container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]')[1];

  it("names the tabs it would bring back, with the key on its own row", () => {
    const { container } = renderDash({ hello: withOffer(["api", "docs", "tests"]) });
    const row = restoreRow(container);
    expect(row.querySelector(".dash-label")!.textContent).toBe("Restore last session 3 tabs · api, docs, tests");
    expect(row.querySelector(".dash-key")!.textContent).toBe("s");
  });
  it("says tab in the singular", () => {
    const { container } = renderDash({ hello: withOffer(["api"]) });
    expect(restoreRow(container).querySelector(".dash-label")!.textContent).toBe("Restore last session 1 tab · api");
  });
  it("names the first three and shows there are more", () => {
    const { container } = renderDash({ hello: withOffer(["a", "b", "c", "d", "e"]) });
    expect(restoreRow(container).querySelector(".dash-label")!.textContent).toBe("Restore last session 5 tabs · a, b, c, …");
  });
  it("cuts a long name to fit", () => {
    const { container } = renderDash({ hello: withOffer(["a-very-long-tab-name-indeed-yes"]) });
    const text = restoreRow(container).querySelector(".dash-label")!.textContent!;
    expect(text).toContain("1 tab · a-very-long-tab-nam…");
    expect(text).not.toContain("indeed");
  });
  it("says in the label when some of them were in bypass, ahead of the names so the count is not read as one", () => {
    const { container } = renderDash({ hello: { ...HELLO, restore: { labels: ["api", "docs"], bypass: 1 } } });
    expect(restoreRow(container).querySelector(".dash-label")!.textContent).toBe(
      "Restore last session 2 tabs, 1 in bypass · api, docs",
    );
  });
  it("says a turn was cut off for as long as the offer stands, and nothing when none was", () => {
    const cut = { labels: ["api"], bypass: 0, cutNote: "a turn was cut off when this window last ended" };
    const { container } = renderDash({ hello: { ...HELLO, restore: cut } });
    expect(container.querySelector(".dash-cut-note")!.textContent).toBe(cut.cutNote);

    const quiet = renderDash({ hello: withOffer(["api"]) });
    expect(quiet.container.querySelector(".dash-cut-note")).toBeNull();
  });
  it("clicking it runs it", () => {
    const { container, onItem } = renderDash({ hello: withOffer(["api"]) });
    fireEvent.click(restoreRow(container));
    expect(onItem).toHaveBeenCalledWith("restore");
  });
});

describe("MODE_CONSEQUENCE", () => {
  /** Auto's consequence describes the policy's acceptEdits fast path. The `?` sheet
   *  (`KeymapOverlay`) carries mode consequences, rather than the start screen. */
  it("says what manual, auto and bypass actually do", () => {
    expect(MODE_CONSEQUENCE.manual).toBe("Safe reads run; every edit and anything else asks.");
    expect(MODE_CONSEQUENCE.auto).toBe("Edits in this project and safe reads run; anything else asks.");
    expect(MODE_CONSEQUENCE.bypass).toBe("Nothing asks: edits and commands run unasked.");
  });
});

describe("Dashboard", () => {
  /** The
   *  where-line no longer names the backend -- it reads "sidecar"/"legacy" in every release build
   *  and carries nothing; the backend stays reachable in `prefix i` (the session-detail route). */
  it("draws the centred question and the where-line with no backend, account omitted when null", () => {
    const { getByText, container } = renderDash();
    // The question and where-line are the whole head; no brand line competes with them.
    getByText("What shall we work on?");
    expect(container.querySelector(".dash-brand")).toBeNull();
    getByText("~/Documents/eitri · work");
    const noAccount = { ...HELLO, account: null };
    const { getByText: getByTextNoAccount } = renderDash({ hello: noAccount });
    getByTextNoAccount("~/Documents/eitri");
    expect(container.textContent).toContain("eitri");
    expect(container.textContent).not.toContain("sidecar");
  });

  it("cuts the cwd and drops the account when narrow, still with no backend", () => {
    const { getByText, queryByText } = renderDash({ narrow: true });
    getByText("~/…/eitri");
    expect(queryByText(/work/)).toBeNull();
    expect(queryByText(/sidecar/)).toBeNull();
  });

  it("draws every item with its key letter and no mode row", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    const { container } = renderDash({ hello: withRecord });
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    expect(items.map((el) => el.querySelector(".dash-key")!.textContent)).toEqual(["i", "r", "w", "?"]);
  });

  it("shows the newest record's title, muted, next to Resume last", () => {
    const withRecord: Hello = {
      ...HELLO,
      resumableSessions: [session({ title: "zsh 补全 compinit slow" }), session({ providerSessionId: "id-2", title: "older" })],
    };
    const { container } = renderDash({ hello: withRecord });
    const resume = container.querySelector('[data-nav-stop="dash"]:nth-child(2)')!;
    expect(resume.textContent).toContain("Resume last");
    const muted = resume.querySelector(".dash-detail")!;
    expect(muted.textContent).toContain("zsh 补全 compinit slow");
    expect(muted.textContent).not.toContain("older");
  });

  it("says when the newest record was last opened, as a time that cannot go stale, before its key", () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(new Date(2026, 9, 28, 15, 30));
      const withRecord: Hello = {
        ...HELLO,
        resumableSessions: [
          session({ updatedAt: String(new Date(2026, 9, 28, 9, 5).getTime()) }),
          session({ providerSessionId: "id-2", updatedAt: "5" }),
        ],
      };
      const { container } = renderDash({ hello: withRecord });
      const resume = container.querySelector('[data-nav-stop="dash"]:nth-child(2)')!;
      expect(resume.querySelector(".dash-when")!.textContent).toBe("09:05");
      // Before the key, outside the label, so the label's ellipsis never cuts it.
      expect(resume.querySelector(".dash-when")!.nextElementSibling!.className).toBe("dash-key");
      expect(resume.querySelector(".dash-label .dash-when")).toBeNull();
      // Only Resume last carries one.
      expect(container.querySelectorAll(".dash-when")).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("stamps a clock time today, a month and day before it, and nothing for a stamp it cannot read", () => {
    const now = new Date(2026, 9, 28, 0, 10).getTime();
    expect(lastOpenedStamp(now, String(new Date(2026, 9, 28, 0, 3).getTime()))).toBe("00:03");
    expect(lastOpenedStamp(now, String(new Date(2026, 9, 27, 23, 59).getTime()))).toBe("Oct 27");
    expect(lastOpenedStamp(now, String(new Date(2025, 11, 31, 12, 0).getTime()))).toBe("Dec 31");
    for (const bad of ["", "  ", "garbage", "0", "-5", "NaN", "Infinity", "8640000000000001"]) expect(lastOpenedStamp(now, bad), bad).toBeNull();
  });

  it("draws no time, and no empty span, when the newest record's stamp cannot be read", () => {
    for (const updatedAt of ["", "not a time"]) {
      const { container, unmount } = renderDash({ hello: { ...HELLO, resumableSessions: [session({ updatedAt })] } });
      expect(container.querySelector(".dash-when"), updatedAt).toBeNull();
      expect(container.textContent).not.toContain("not a time");
      unmount();
    }
  });

  it("leaves the time out on a narrow panel, so the title keeps the room", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session({ updatedAt: String(Date.now() - 60 * 60_000) })] };
    const { container } = renderDash({ hello: withRecord, narrow: true });
    expect(container.querySelector(".dash-when")).toBeNull();
    expect(container.querySelector(".dash-detail")!.textContent).toBe("zsh 补全 compinit slow");
  });

  it("draws a title or tab name that holds markup as literal text, and leaves the cutting to CSS", () => {
    const longTitle = `<b>x</b> <img src=x onerror=alert(1)> ${"long words ".repeat(20)}end`;
    const hello: Hello = {
      ...HELLO,
      resumableSessions: [session({ title: longTitle })],
      restore: { labels: ["<i>tab</i>"], bypass: 0 },
    };
    const { container } = renderDash({ hello });
    for (const detail of Array.from(container.querySelectorAll(".dash-detail"))) {
      expect(detail.querySelector("b, i, img")).toBeNull();
    }
    const [restore, resume] = Array.from(container.querySelectorAll(".dash-detail"));
    expect(restore.textContent).toBe("1 tab · <i>tab</i>");
    // The whole title is in the row; nothing in the component shortened it.
    expect(resume.textContent).toBe(longTitle);
  });

  /* The start screen carries no hints of its own. The status band under it names the mode, and
     the `?` sheet (the last item) says what a mode does, what a message carries and the window's keys. */
  it("draws nothing but the name, the where-line and the items: no mode row and no notes under them", () => {
    for (const mode of ["auto", "bypass"] as const) {
      const { container, unmount } = renderDash({ mode });
      expect(container.querySelector(".mode-glyph")).toBeNull();
      expect(container.textContent).not.toMatch(/Mode:/);
      for (const gone of [".dash-consequence", ".dash-context", ".dash-hint"]) {
        expect(container.querySelector(gone), gone).toBeNull();
      }
      expect(container.textContent).not.toContain(MODE_CONSEQUENCE[mode]);
      expect(Array.from(container.querySelector(".dashboard")!.children).map((el) => el.className)).toEqual([
        "dash-head",
        "dash-items",
      ]);
      // The head is the mark, the question and the where-line, and nothing more.
      expect(Array.from(container.querySelector(".dash-head")!.children).map((el) => el.getAttribute("class"))).toEqual([
        "dash-mark",
        "dash-title",
        "dash-where",
      ]);
      unmount();
    }
  });

  /* The `?` sheet carries mode consequences; the band names the mode, so the start screen needs no mode row. */
  it("says every edit asks in manual, and on another agent's tab only what reaches Eitri", () => {
    const { container } = renderDash({ mode: "manual" });
    expect(container.textContent).not.toMatch(/Mode:/);
    expect(modeConsequence("manual", null, "Ctrl+b")).toBe("Safe reads run; every edit and anything else asks.");
    expect(modeConsequence("manual", "codex", "Ctrl+b")).toBe(
      "Every request codex makes asks; what it runs without asking is in Ctrl+b i.",
    );
    expect(modeConsequence("auto", "codex", "Ctrl+b")).toBe(MODE_CONSEQUENCE.auto);
  });

  it("marks the item under cursor with aria-current, and nothing else", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    const { container } = renderDash({ hello: withRecord, cursor: 2 });
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    expect(items.map((el) => el.getAttribute("aria-current"))).toEqual([null, null, "true", null]);
  });

  it("items are div role=button with tabIndex=-1, never a real <button>", () => {
    const { container } = renderDash();
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    expect(items.length).toBeGreaterThan(0);
    for (const item of items) {
      expect(item.tagName).toBe("DIV");
      expect(item.getAttribute("role")).toBe("button");
      expect(item.tabIndex).toBe(-1);
    }
  });

  it("a click runs the item under it", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    const { container, onItem } = renderDash({ hello: withRecord });
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    fireEvent.click(items[1]);
    expect(onItem).toHaveBeenCalledWith("resume");
  });
});

/** A companion window with no editor attached is the one case the start screen says a line under its items:
 *  it is something to fix (attach an editor), not something to learn, so it is said where it is seen. With
 *  an editor attached, or in the one-window mode, nothing is said. */
describe("Dashboard: the companion line", () => {
  it("says how to attach an editor while none is attached", () => {
    for (const state of ["none", "attaching", "detached", "failed"] as const) {
      const { container, unmount } = renderDash({ editorLink: { state, text: "x" } });
      expect(container.querySelector(".dash-context")!.textContent).toBe(
        "No editor is attached: run :EitriPanel in nvim to send its file with each message.",
      );
      expect(container.querySelector(".dash-hint")).toBeNull();
      unmount();
    }
  });

  it("says nothing with an editor attached, or in the one-window mode", () => {
    expect(renderDash({ editorLink: { state: "attached", text: "" } }).container.querySelector(".dash-context")).toBeNull();
    expect(renderDash({ editorLink: null }).container.querySelector(".dash-context")).toBeNull();
  });
});
