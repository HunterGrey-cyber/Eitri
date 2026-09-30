// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Dashboard, MODE_CONSEQUENCE, dashItems } from "./Dashboard";
import type { DashItem } from "./Dashboard";
import type { Hello, PermissionModeChoice, ResumableSession } from "../types";

afterEach(cleanup);

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/Documents/eitri",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: "28a5e4c",
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
  over: { hello?: Hello; mode?: PermissionModeChoice; cursor?: number; narrow?: boolean; onItem?: (i: DashItem) => void; prefix?: string } = {},
) {
  const onItem = over.onItem ?? vi.fn();
  const props = {
    hello: over.hello ?? HELLO,
    mode: over.mode ?? "auto",
    cursor: over.cursor ?? 0,
    narrow: over.narrow ?? false,
    onItem,
    prefix: over.prefix ?? "Ctrl+b",
  };
  return { onItem, ...render(<Dashboard {...props} />) };
}

describe("dashItems (spec §7)", () => {
  it("omits resume with no records", () => {
    expect(dashItems(HELLO)).toEqual(["new", "sessions", "mode", "keys"]);
  });
  it("offers resume once a record exists", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    expect(dashItems(withRecord)).toEqual(["new", "resume", "sessions", "mode", "keys"]);
  });
});

describe("MODE_CONSEQUENCE (spec §7, decision 3)", () => {
  /** v1 trial item 4C (owner trial feedback §4b, "C: say what the mode is"): auto's line describes
   *  the acceptEdits fast path (item 4A), which this build's policy runs since 4A merged here -- see
   *  `MODE_CONSEQUENCE`'s own doc comment. bypass's line is untouched. */
  it("says what auto and bypass actually do", () => {
    expect(MODE_CONSEQUENCE.auto).toBe("Edits in this project and safe reads run; anything else asks.");
    expect(MODE_CONSEQUENCE.bypass).toBe("Nothing asks: edits and commands run unasked.");
  });
});

describe("Dashboard (panel round 2 plan, Task 12; spec §7)", () => {
  /** v1 trial item 1 (owner: "⏵⏵ auto · sidecar，这个东西应该出现在all session的选择上吗"): the
   *  where-line no longer names the backend -- it reads "sidecar"/"legacy" in every release build
   *  and carries nothing; the backend stays reachable in `prefix i` (decision 6, unchanged). */
  it("draws the centred name and the where-line with no backend, account omitted when null", () => {
    const { getByText, container } = renderDash();
    getByText("Eitri");
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

  it("draws every item with its key letter (spec §7's table); mode reads ⇧Tab, not a letter (V1 S2)", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    const { container } = renderDash({ hello: withRecord });
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    expect(items.map((el) => el.querySelector(".dash-key")!.textContent)).toEqual(["i", "r", "w", "⇧Tab", "?"]);
  });

  it("shows the newest record's title, muted, next to Resume last", () => {
    const withRecord: Hello = {
      ...HELLO,
      resumableSessions: [session({ title: "zsh 补全 compinit slow" }), session({ providerSessionId: "id-2", title: "older" })],
    };
    const { container } = renderDash({ hello: withRecord });
    const resume = container.querySelector('[data-nav-stop="dash"]:nth-child(2)')!;
    expect(resume.textContent).toContain("Resume last");
    const muted = resume.querySelector(".dash-resume-title")!;
    expect(muted.textContent).toContain("zsh 补全 compinit slow");
    expect(muted.textContent).not.toContain("older");
  });

  it("draws Mode: auto with a .mode-glyph", () => {
    const { container, getByText } = renderDash({ mode: "auto" });
    getByText(/Mode: auto/);
    const glyph = container.querySelector(".mode-glyph")!;
    expect(glyph).not.toBeNull();
    expect(glyph.getAttribute("data-mode-name")).toBe("auto");
    expect(glyph.textContent).toBe("⏵⏵");
  });

  it("draws the consequence line for auto and bypass, and none for an unknown mode", () => {
    const { getByText } = renderDash({ mode: "auto" });
    getByText("Edits in this project and safe reads run; anything else asks.");
    const bypass = renderDash({ mode: "bypass" });
    bypass.getByText("Nothing asks: edits and commands run unasked.");
    const unknown = renderDash({ mode: "future-mode" as PermissionModeChoice });
    expect(unknown.container.querySelector(".dash-consequence")).toBeNull();
  });

  it("marks the item under cursor with aria-current, and nothing else", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    const { container } = renderDash({ hello: withRecord, cursor: 2 });
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    expect(items.map((el) => el.getAttribute("aria-current"))).toEqual([null, null, "true", null, null]);
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

/** R13 (v1 picks Task 11, owner decision d): every turn carries what the editor is showing, and this
 *  is where a person picks the mode and starts a session, so it says what goes with each message. What
 *  goes is the editor's file name, or the lines selected in Visual mode -- never the file's text and
 *  never the cursor (`core/src/editor_context/feed.rs` parses `line` and drops it), so the sentence
 *  names those two and nothing else. It is a fact about every turn, not about the mode. */
describe("Dashboard's context line (v1 picks Task 11, R13)", () => {
  const CONTEXT_LINE = "Each message also sends the editor's file name, or the lines you selected there in Visual mode.";

  it("says what each message carries from the editor, once, for auto and bypass", () => {
    for (const mode of ["auto", "bypass"] as const) {
      const { container } = renderDash({ mode });
      const lines = container.querySelectorAll(".dash-context");
      expect(lines).toHaveLength(1);
      expect(lines[0].textContent).toBe(CONTEXT_LINE);
    }
  });

  it("is drawn under the consequence line and above the first-run hint, which stays last", () => {
    const { container } = renderDash({ mode: "auto" });
    const consequence = container.querySelector(".dash-consequence")!;
    const context = container.querySelector(".dash-context")!;
    const hint = container.querySelector(".dash-hint")!;
    expect(consequence.nextElementSibling).toBe(context);
    expect(context.nextElementSibling).toBe(hint);
    expect(hint.nextElementSibling).toBeNull();
  });

  it("does not hinge on the mode: a mode with no consequence line still says it", () => {
    const { container } = renderDash({ mode: "future-mode" as PermissionModeChoice });
    expect(container.querySelector(".dash-consequence")).toBeNull();
    expect(container.querySelector(".dash-context")!.textContent).toBe(CONTEXT_LINE);
  });

  it("is drawn in a narrow panel too, in the same words", () => {
    const { container } = renderDash({ narrow: true });
    expect(container.querySelector(".dash-context")!.textContent).toBe(CONTEXT_LINE);
  });
});

describe("Dashboard's first-run hint line (V1 P11, spec §10.1)", () => {
  it("shows the stock Ctrl+b prefix by default", () => {
    const { container } = renderDash();
    const hint = container.querySelector(".dash-hint")!;
    expect(hint.textContent).toBe(
      "Ctrl+h / Ctrl+l  editor ⇄ chat · i or Ctrl+j  type · ctrl+c  interrupt · Ctrl+b  window keys · ?  all keys",
    );
  });

  it("reads a configured prefix instead", () => {
    const { container } = renderDash({ prefix: "Ctrl+a" });
    const hint = container.querySelector(".dash-hint")!;
    expect(hint.textContent).toBe(
      "Ctrl+h / Ctrl+l  editor ⇄ chat · i or Ctrl+j  type · ctrl+c  interrupt · Ctrl+a  window keys · ?  all keys",
    );
  });

  it("is drawn on every render, not gated behind any first-run state", () => {
    // No prop for "seen before" exists to gate this -- it is unconditional, per the spec's own
    // "shown on every empty tab" (this screen is seen only when nothing else is, so it costs
    // nothing and needs no first-run state file).
    const first = renderDash();
    expect(first.container.querySelector(".dash-hint")).not.toBeNull();
    const second = renderDash();
    expect(second.container.querySelector(".dash-hint")).not.toBeNull();
  });
});
