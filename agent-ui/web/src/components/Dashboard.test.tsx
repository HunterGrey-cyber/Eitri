// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Dashboard, MODE_CONSEQUENCE, dashItems } from "./Dashboard";
import type { DashItem } from "./Dashboard";
import type { Hello, PermissionModeChoice, ResumableSession } from "../types";

afterEach(cleanup);

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/Documents/neovibe",
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

describe("dashItems", () => {
  it("omits resume with no records", () => {
    expect(dashItems(HELLO)).toEqual(["new", "sessions", "mode", "keys"]);
  });
  it("offers resume once a record exists", () => {
    const withRecord: Hello = { ...HELLO, resumableSessions: [session()] };
    expect(dashItems(withRecord)).toEqual(["new", "resume", "sessions", "mode", "keys"]);
  });
});

describe("MODE_CONSEQUENCE", () => {
  /** Auto's text describes the acceptEdits fast path used by this build's policy. The bypass text
   *  is unchanged. */
  it("says what auto and bypass actually do", () => {
    expect(MODE_CONSEQUENCE.auto).toBe("Edits in this project and safe reads run; anything else asks.");
    expect(MODE_CONSEQUENCE.bypass).toBe("Nothing asks: edits and commands run unasked.");
  });
});

describe("Dashboard", () => {
  /** The where-line does not name the backend: the name is the same in every release build, so
   *  it carries nothing. The backend stays reachable in `prefix i`. */
  it("draws the centred name and the where-line with no backend, account omitted when null", () => {
    const { getByText, container } = renderDash();
    getByText("neovibe");
    getByText("~/Documents/neovibe · work");
    const noAccount = { ...HELLO, account: null };
    const { getByText: getByTextNoAccount } = renderDash({ hello: noAccount });
    getByTextNoAccount("~/Documents/neovibe");
    expect(container.textContent).toContain("neovibe");
    expect(container.textContent).not.toContain("sidecar");
  });

  it("cuts the cwd and drops the account when narrow, still with no backend", () => {
    const { getByText, queryByText } = renderDash({ narrow: true });
    getByText("~/…/neovibe");
    expect(queryByText(/work/)).toBeNull();
    expect(queryByText(/sidecar/)).toBeNull();
  });

  it("draws every item with its key letter; mode reads ⇧Tab, not a letter", () => {
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

describe("Dashboard's first-run hint line", () => {
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
    // Show the hint on every empty tab. No first-run state or seen-before prop is needed.
    const first = renderDash();
    expect(first.container.querySelector(".dash-hint")).not.toBeNull();
    const second = renderDash();
    expect(second.container.querySelector(".dash-hint")).not.toBeNull();
  });
});
