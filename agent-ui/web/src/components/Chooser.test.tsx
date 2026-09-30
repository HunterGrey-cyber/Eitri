// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Chooser } from "./Chooser";
import type { ChooserEnvelope, TabInfo } from "../types";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const TAB1: TabInfo = {
  id: 1, number: 1, label: "1 fix-parser", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null,
};
const TAB2: TabInfo = {
  id: 2, number: 2, label: "2 legacy", name: null, state: "live", mode: "bypass",
  marker: null, pending: 0, resumable: true, failure: null, title: null,
};

const ENVELOPE: ChooserEnvelope = {
  open: [
    { tab: 1, label: "1 fix-parser", marker: null, pending: 0, resumable: true },
    { tab: 2, label: "2 legacy", marker: null, pending: 0, resumable: true },
  ],
  records: [
    { providerSessionId: "held-0000", name: null, title: "held one", createdAt: "1", updatedAt: "2", heldElsewhere: true },
    { providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false },
  ],
};
const TABS = [TAB1, TAB2];

function renderChooser(over: Record<string, unknown> = {}) {
  const props = {
    envelope: ENVELOPE,
    tabs: TABS,
    active: null,
    defaultMode: "auto" as const,
    projectDir: "/home/user/src/neovibe",
    newTabChord: "Ctrl+b c",
    focusRequest: 0,
    onSwitch: vi.fn(),
    onResume: vi.fn(),
    onNewSession: vi.fn(),
    onCloseTab: vi.fn(),
    onRenameTab: vi.fn(),
    onCycleMode: vi.fn(),
    onCycleTabMode: vi.fn(),
    onLeave: vi.fn(),
    // v1 (spec §3.4): claims nothing by default, so every existing test below exercises the
    // chooser's own keys exactly as before; the dedicated describe further down overrides this to
    // prove the opposite -- a claiming `answerConfirm` swallows the chooser's keys whole.
    answerConfirm: vi.fn(() => false),
    ...over,
  };
  const view = render(<Chooser {...props} />);
  const root = view.container.querySelector<HTMLElement>(".chooser")!;
  return { props, root, ...view };
}

describe("Chooser", () => {
  /** v1 polish item 8: a tab holding cards read `⚑ · idle`. */
  it("an open tab with waiting cards reads ⚑N waiting, not idle", () => {
    const waiting = { ...TAB1, marker: "needs_input" as const, pending: 2 };
    const { container } = renderChooser({
      tabs: [waiting, TAB2],
      envelope: { ...ENVELOPE, open: [{ ...ENVELOPE.open[0], marker: "needs_input", pending: 2 }, ENVELOPE.open[1]] },
    });
    const right = [...container.querySelectorAll(".chooser-right")].map((n) => n.textContent);
    expect(right).toContain("⚑2 waiting");
    expect(right.join("|")).not.toContain("⚑2 · idle");
  });
  it("takes the keys when it opens; j/k move onto an open tab and Enter switches to it", () => {
    const { root, props } = renderChooser();
    expect(root.contains(document.activeElement)).toBe(true);
    fireEvent.keyDown(root, { key: "j" }); // New session -> tab 1
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onSwitch).toHaveBeenCalledWith(1);
  });
  it("a record held elsewhere cannot be chosen, and the next one can", () => {
    const { root, props } = renderChooser();
    // rows: New session(0), tab1(1), tab2(2), held(3), free(4)
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onResume).not.toHaveBeenCalled();
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onResume).toHaveBeenCalledWith("free-0000");
  });
  it("starts on the active tab's row", () => {
    const { container } = renderChooser({ active: 2 });
    expect(container.querySelector(".chooser-row.current")!.textContent).toContain("2 legacy");
  });
  it("x asks to close the open tab under the cursor", () => {
    const { root, props } = renderChooser({ active: 1 });
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onCloseTab).toHaveBeenCalledWith(1);
  });
  it("x on New session or a record row does nothing -- it is only bound on an open tab", () => {
    // Fix round 1, blocking finding 1: `x` used to fall into the catch-all `onLeave` branch for
    // any row that is not a tab, silently closing the whole chooser even though the keys line
    // advertises `x close tab` only on an open tab's row.
    const { root, props } = renderChooser(); // cursor starts on "New session" (active: null)
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onLeave).not.toHaveBeenCalled();
    expect(props.onCloseTab).not.toHaveBeenCalled();
    // Move onto a record row (New session, tab1, tab2, held, free) and try again.
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onLeave).not.toHaveBeenCalled();
    expect(props.onCloseTab).not.toHaveBeenCalled();
  });
  it("Esc and q leave with no arguments (wave 4 R2: the chooser no longer says which chooser this was)", () => {
    const first = renderChooser();
    fireEvent.keyDown(first.root, { key: "Escape" });
    expect(first.props.onLeave).toHaveBeenCalledWith();
    cleanup();
    const second = renderChooser();
    fireEvent.keyDown(second.root, { key: "q" });
    expect(second.props.onLeave).toHaveBeenCalledWith();
  });
  it("/ opens a filter; typing narrows the rows and Enter returns to the list", () => {
    const { root, container } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(document.activeElement).toBe(filter);
    fireEvent.change(filter, { target: { value: "free" } });
    expect(container.querySelectorAll(".chooser-row").length).toBe(1);
    fireEvent.keyDown(filter, { key: "Enter" });
    expect(document.activeElement).not.toBe(filter);
  });
  /** Review focus 5. */
  it("the_filter_ignores_enter_while_composing", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.keyDown(filter, { key: "Enter", isComposing: true });
    fireEvent.keyDown(filter, { key: "Escape", isComposing: true });
    expect(document.activeElement).toBe(filter);
    expect(props.onLeave).not.toHaveBeenCalled();
  });

  it("the header shows the project name and the open/earlier counts, and 'N of M' while filtered", () => {
    const { root, container } = renderChooser();
    expect(container.querySelector(".chooser-title")!.textContent).toBe("Sessions");
    expect(container.querySelector(".chooser-project")!.textContent).toBe("neovibe");
    expect(container.querySelector(".chooser-counts")!.textContent).toBe("2 open · 2 earlier");
    fireEvent.keyDown(root, { key: "/" });
    fireEvent.change(container.querySelector<HTMLInputElement>(".chooser-filter")!, { target: { value: "free" } });
    expect(container.querySelector(".chooser-counts")!.textContent).toBe("1 of 4");
  });

  it("groups open tabs under 'Open in this window' and records under 'Earlier in <project>'", () => {
    const { container } = renderChooser();
    const headers = Array.from(container.querySelectorAll(".chooser-group")).map((el) => el.textContent);
    expect(headers).toEqual(["Open in this window", "Earlier in neovibe"]);
  });

  it("j/k skip group headers -- every step lands on a real row", () => {
    const { root, container } = renderChooser();
    for (let i = 0; i < 4; i += 1) fireEvent.keyDown(root, { key: "j" });
    const current = container.querySelector(".chooser-row.current")!;
    expect(current.classList.contains("chooser-group")).toBe(false);
    expect(container.querySelectorAll(".chooser-row").length).toBe(5); // new + 2 tabs + 2 records
  });

  it("New session shows the new-tab chord, and Enter on it posts a new session", () => {
    const { root, props, container } = renderChooser();
    expect(container.querySelector(".chooser-row")!.textContent).toContain("Ctrl+b c");
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onNewSession).toHaveBeenCalled();
  });

  /** v1 trial item 1 (owner: "⏵⏵ auto · sidecar，这个东西应该出现在all session的选择上吗"): the backend
   *  name is no longer part of a record row -- only the short id, which is how an untitled record
   *  is told apart from another. */
  it("a record row's line 2 keeps only the short id, never the backend, and is untitled with neither name nor title", () => {
    const { container } = renderChooser({
      envelope: {
        open: [],
        records: [
          { providerSessionId: "aaaa1111bbbb", name: null, title: null, createdAt: "1", updatedAt: "2", heldElsewhere: false },
        ],
      },
    });
    const row = container.querySelectorAll(".chooser-row")[1]; // New session, then the record
    expect(row.querySelector(".chooser-line2")!.textContent).toBe("aaaa1111");
    expect(row.textContent).toContain("untitled");
  });

  it("a held record shows ⊘ and says it can't be resumed here, and is not choosable", () => {
    const { root, container, props } = renderChooser();
    const held = Array.from(container.querySelectorAll(".chooser-row")).find((r) => r.textContent?.includes("held one"))!;
    expect(held.querySelector(".chooser-sign")!.textContent).toBe("⊘");
    expect(held.querySelector(".chooser-line2")!.textContent).toBe("open in another window, can't be resumed here");
    expect(held.classList.contains("unchoosable")).toBe(true);
    // Land on it and try to choose it: nothing fires.
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onResume).not.toHaveBeenCalled();
  });

  it("<mark> wraps a filter match", () => {
    const { root, container } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    fireEvent.change(container.querySelector<HTMLInputElement>(".chooser-filter")!, { target: { value: "fix" } });
    const mark = container.querySelector("mark")!;
    expect(mark).not.toBeNull();
    expect(mark.textContent!.toLowerCase()).toBe("fix");
  });

  /** v1 D6 made an open tab's mode switchable, so the line no longer says "fixed" (fix round 1): it
   *  says what Shift+Tab does from here, and "toggle" rather than "cycle" (O2 a). */
  it("the mode line reads Start/Resume on New session and a record, and what Shift+Tab does on an open tab", () => {
    const { root, container } = renderChooser({ defaultMode: "auto" });
    const line = () => container.querySelector(".chooser-mode-line")!.textContent;
    // cursor starts on New session
    expect(line()).toBe("Start in ⏵⏵ auto mode on (shift+tab to toggle)");
    fireEvent.keyDown(root, { key: "j" }); // tab 1, auto, not the active tab (active: null)
    expect(line()).toBe("Tab 1 runs in ⏵⏵ auto mode on · switch to it to change");
    fireEvent.keyDown(root, { key: "j" }); // tab 2, bypass: leaving works from anywhere
    expect(line()).toBe("Tab 2 runs in ⏵⏵ bypass mode on (shift+tab to toggle)");
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" }); // a record
    expect(line()).toBe("Resume in ⏵⏵ auto mode on (shift+tab to toggle)");
  });

  it("the active tab's own row offers the toggle", () => {
    const { container } = renderChooser({ active: 1 }); // starts on the active tab's row
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("Tab 1 runs in ⏵⏵ auto mode on (shift+tab to toggle)");
  });

  it("an ended auto tab's row says it has ended, and Shift+Tab there flashes r rather than asking Rust", () => {
    const ended: TabInfo = { ...TAB1, state: "ended" };
    const { root, container, props } = renderChooser({ active: 1, tabs: [ended, TAB2] });
    const line = () => container.querySelector(".chooser-mode-line")!.textContent;
    expect(line()).toBe("Tab 1 runs in ⏵⏵ auto mode on · the session has ended");
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(props.onCycleTabMode).not.toHaveBeenCalled();
    expect(line()).toBe("the session has ended — r to start again");
  });

  it("Shift+Tab: New session and a record cycle the resume mode; an open tab's row toggles that tab where Rust would", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true }); // on New session
    expect(props.onCycleMode).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(root, { key: "j" }); // tab 1: auto, and not the one on screen
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(props.onCycleMode).toHaveBeenCalledTimes(1); // not called again
    expect(props.onCycleTabMode).not.toHaveBeenCalled();
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("switch to tab 1 to change its mode");
    fireEvent.keyDown(root, { key: "j" }); // tab 2: bypass, which can always be left
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(props.onCycleTabMode).toHaveBeenCalledWith(2);
  });

  it("Shift+Tab on the active tab's own row toggles it (entering bypass: Rust asks, answered in here)", () => {
    const { root, props } = renderChooser({ active: 1 });
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(props.onCycleTabMode).toHaveBeenCalledWith(1);
    expect(props.onCycleMode).not.toHaveBeenCalled();
  });

  it("Shift+Tab as WebKitGTK delivers it from a real keyboard (key Unidentified, code Tab) cycles too", () => {
    const { root, props } = renderChooser();
    fireEvent.keyDown(root, { key: "Unidentified", code: "Tab", keyCode: 9, shiftKey: true });
    expect(props.onCycleMode).toHaveBeenCalledTimes(1);
  });

  it("Ctrl+r on an open tab opens its inline rename, posting rename_tab through onRenameTab on Enter", () => {
    const { root, container, props } = renderChooser({ active: 1 });
    fireEvent.keyDown(root, { key: "r", ctrlKey: true });
    const field = container.querySelector<HTMLInputElement>(".chooser-rename")!;
    expect(document.activeElement).toBe(field);
    fireEvent.change(field, { target: { value: "notes" } });
    fireEvent.keyDown(field, { key: "Enter" });
    expect(props.onRenameTab).toHaveBeenCalledWith(1, "notes");
    expect(container.querySelector(".chooser-rename")).toBeNull();
  });

  it("Ctrl+r on a record does nothing", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "j" }); // tab 1
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" }); // a record row
    fireEvent.keyDown(root, { key: "r", ctrlKey: true });
    expect(container.querySelector(".chooser-rename")).toBeNull();
    expect(props.onRenameTab).not.toHaveBeenCalled();
  });

  it("the keys line changes per row: an open tab offers rename/close, New session and a record do not", () => {
    const { root, container } = renderChooser();
    expect(container.querySelector(".chooser-keys")!.textContent).toBe("enter resume · / filter · shift+tab mode · esc");
    fireEvent.keyDown(root, { key: "j" }); // tab 1
    expect(container.querySelector(".chooser-keys")!.textContent).toBe("enter switch · / filter · ctrl+r rename · x close tab · shift+tab mode · esc");
  });

  /** v1 trial item 1: an open tab's line 2 used to read `⏵⏵ <mode> · <backend>[ · <title>]`
   *  unconditionally -- the backend name (which reads "sidecar" in every release build) carried
   *  nothing, and the mode was shown even for the default, auto. */
  describe("an open tab's line 2 (v1 trial item 1)", () => {
    it("is absent in auto mode with no title -- no mode, no backend", () => {
      const { container } = renderChooser(); // TAB1: auto, title null
      const rows = container.querySelectorAll(".chooser-row");
      expect(rows[1].querySelector(".chooser-line2")).toBeNull();
      expect(rows[1].textContent).not.toContain("sidecar");
    });
    it("carries only the title in auto mode -- no mode, no backend", () => {
      const withTitle = { ...TAB1, title: "write the docs" };
      const { container } = renderChooser({ tabs: [withTitle, TAB2] });
      const rows = container.querySelectorAll(".chooser-row");
      const line2 = rows[1].querySelector(".chooser-line2")!;
      expect(line2.textContent).toBe("write the docs");
    });
    it("shows only the bypass notice, drawn with the shared bypass mode-glyph token, when bypass and no title", () => {
      const { container } = renderChooser(); // TAB2: bypass, title null
      const rows = container.querySelectorAll(".chooser-row");
      const line2 = rows[2].querySelector(".chooser-line2")!;
      expect(line2.textContent).toBe("⏵⏵ bypass");
      const glyph = line2.querySelector(".mode-glyph")!;
      expect(glyph.getAttribute("data-mode-name")).toBe("bypass");
      expect(rows[2].textContent).not.toContain("sidecar");
    });
    it("joins the bypass notice and the title with · when both are present", () => {
      const withTitle = { ...TAB2, title: "write the docs" };
      const { container } = renderChooser({ tabs: [TAB1, withTitle] });
      const rows = container.querySelectorAll(".chooser-row");
      const line2 = rows[2].querySelector(".chooser-line2")!;
      expect(line2.textContent).toBe("⏵⏵ bypass · write the docs");
    });
  });

  /** v1 trial item 1: a not-started tab's row used to read "new" twice -- once as its default
   *  label, once as its state word on the right. */
  it("a not-started tab has no state word on the right", () => {
    const notStarted: TabInfo = { ...TAB1, state: "not_started" };
    const { container } = renderChooser({
      tabs: [notStarted, TAB2],
      envelope: { ...ENVELOPE, open: [{ ...ENVELOPE.open[0], label: "1 new" }, ENVELOPE.open[1]] },
    });
    const rows = container.querySelectorAll(".chooser-row");
    expect(rows[1].querySelector(".chooser-lead")!.textContent).toBe("1 new");
    expect(rows[1].querySelector(".chooser-right")!.textContent).toBe("");
  });

  it("Space does nothing -- no preview, decision 6", () => {
    const { root, props } = renderChooser();
    fireEvent.keyDown(root, { key: " " });
    expect(props.onSwitch).not.toHaveBeenCalled();
    expect(props.onResume).not.toHaveBeenCalled();
    expect(props.onNewSession).not.toHaveBeenCalled();
    expect(props.onCycleMode).not.toHaveBeenCalled();
  });

  /** Wave 3 Task 1: `App` bumps `focusRequest` on `pane_focus`/`arrive` so a chooser that lost DOM
   *  focus (a GTK round trip) gets it back, without re-selecting a half-typed filter/rename value. */
  describe("focusRequest (wave 3 Task 1)", () => {
    it("re-focuses the root after focus moved elsewhere", () => {
      const outside = document.createElement("button");
      document.body.appendChild(outside);
      const { root, props, rerender } = renderChooser();
      outside.focus();
      expect(document.activeElement).toBe(outside);
      rerender(<Chooser {...props} focusRequest={1} />);
      expect(document.activeElement).toBe(root);
      document.body.removeChild(outside);
    });

    it("re-focuses the filter input, not the root, while filtering", () => {
      const outside = document.createElement("button");
      document.body.appendChild(outside);
      const { root, container, props, rerender } = renderChooser({ focusRequest: 1 });
      fireEvent.keyDown(root, { key: "/" });
      const filterInput = container.querySelector<HTMLInputElement>(".chooser-filter")!;
      expect(document.activeElement).toBe(filterInput);
      outside.focus();
      expect(document.activeElement).toBe(outside);
      rerender(<Chooser {...props} focusRequest={2} />);
      expect(document.activeElement).toBe(container.querySelector<HTMLInputElement>(".chooser-filter"));
      document.body.removeChild(outside);
    });
  });
  /** Spec §6.2: `gg`/`G` go to the ends. The r2-gui GUI pass (2026-09-26) found neither bound,
   *  and the `g`s went on to the conversation under the chooser. */
  it("gg goes to the first row and G to the last, and neither reaches the panel under it", () => {
    // The panel under it is a React `onKeyDown` on an ancestor (`App.tsx`'s root), so the check is
    // one too.
    const outer = vi.fn((e: { key: string }) => e.key);
    const { root, props, rerender } = renderChooser();
    rerender(
      <div onKeyDown={(e) => outer({ key: e.key })}>
        <Chooser {...props} />
      </div>,
    );
    const current = () => document.querySelector(".chooser-row.current")!.textContent;
    void root;
    const el = () => document.querySelector<HTMLElement>(".chooser")!;
    fireEvent.keyDown(el(), { key: "G" });
    expect(current()).toContain("free one");
    fireEvent.keyDown(el(), { key: "g" });
    fireEvent.keyDown(el(), { key: "g" });
    expect(current()).toContain("New session");
    // A lone `g` then another key is not `gg` -- and (R1, K01) that key is swallowed, not run: the `j`
    // after the `g` moves nothing, and only the wait is over.
    fireEvent.keyDown(el(), { key: "j" });
    fireEvent.keyDown(el(), { key: "g" });
    fireEvent.keyDown(el(), { key: "j" });
    expect(current()).toContain("1 fix-parser");
    fireEvent.keyDown(el(), { key: "j" });
    expect(current()).toContain("2 legacy");
    expect(outer).not.toHaveBeenCalledWith(expect.objectContaining({ key: "g" }));
    expect(outer).not.toHaveBeenCalledWith(expect.objectContaining({ key: "G" }));
  });
  /** rc.3 minors review: a `g` held with Ctrl, Alt or Meta is a chord meant for something else; it must
   *  not arm the chooser's `gg` wait and so swallow the next plain key. */
  it("a modified g does not arm gg, so the next j still moves", () => {
    const { root } = renderChooser();
    void root;
    const current = () => document.querySelector(".chooser-row.current")!.textContent;
    const el = () => document.querySelector<HTMLElement>(".chooser")!;
    for (const mod of [{ altKey: true }, { metaKey: true }, { ctrlKey: true }]) {
      fireEvent.keyDown(el(), { key: "G" });
      fireEvent.keyDown(el(), { key: "g" });
      fireEvent.keyDown(el(), { key: "g" });
      expect(current()).toContain("New session");
      fireEvent.keyDown(el(), { key: "g", ...mod });
      fireEvent.keyDown(el(), { key: "j" });
      expect(current()).toContain("1 fix-parser");
    }
  });
  /** Spec §6.1: the current row's sign cell is the panel's solid cursor holding `›`. The r2-gui GUI
   *  pass saw only the cursorline fill. */
  it("draws the current row's sign as › and no other row's", () => {
    const { root } = renderChooser();
    const signs = Array.from(root.querySelectorAll(".chooser-row .chooser-sign")).map((el) => el.textContent);
    expect(signs.filter((t) => t === "›").length).toBe(1);
    expect(root.querySelector(".chooser-row.current .chooser-sign")!.textContent).toBe("›");
  });

  /** v1 (spec §3.4): a bypass (or window-close) confirm owns every key ahead of everything else in
   *  the panel, the chooser included -- `answerConfirm` is tried first in `onKeyDown`, and when it
   *  claims the key nothing below (the row cursor, Enter's `choose`, `x`, `q`/`Esc`, this
   *  component's own Shift+Tab) may also react to the same keydown. */
  describe("a claiming answerConfirm owns the key first (v1, spec §3.4)", () => {
    it.each(["j", "k", "Enter", "x", "q", "Escape"])("%s does nothing while it is claimed", (key) => {
      const answerConfirm = vi.fn(() => true);
      const { root, container, props } = renderChooser({ answerConfirm, active: 1 });
      const before = container.querySelector(".chooser-row.current")!.textContent;
      fireEvent.keyDown(root, { key });
      expect(answerConfirm).toHaveBeenCalledTimes(1);
      expect(container.querySelector(".chooser-row.current")!.textContent).toBe(before);
      expect(props.onSwitch).not.toHaveBeenCalled();
      expect(props.onResume).not.toHaveBeenCalled();
      expect(props.onCloseTab).not.toHaveBeenCalled();
      expect(props.onLeave).not.toHaveBeenCalled();
    });

    it("Shift+Tab does nothing while it is claimed either", () => {
      const answerConfirm = vi.fn(() => true);
      const { root, props } = renderChooser({ answerConfirm });
      fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
      expect(answerConfirm).toHaveBeenCalledTimes(1);
      expect(props.onCycleMode).not.toHaveBeenCalled();
    });
  });
});

/** v1 picks, Task 10 (K03, R1, R12). K03 (kbux 2026-09-29, S07.4b): a filter that matched nothing left no
 *  row under the cursor, `modeLine` read `row.kind` on `undefined`, and the throw took the whole panel
 *  with it. The list keys are R12's `↓` `↑` `Ctrl+n` `Ctrl+p`; the chooser's own `g` follows R1. */
describe("Chooser: K03 and the list keys (v1 picks R1, R12)", () => {
  const currentRow = (c: HTMLElement) => c.querySelector(".chooser-row.current")?.textContent ?? null;
  /** ↓ ↑ Ctrl+n Ctrl+p, as the four keydowns a real keyboard sends. */
  const FOUR = [
    { key: "ArrowDown" },
    { key: "ArrowUp" },
    { key: "n", ctrlKey: true },
    { key: "p", ctrlKey: true },
  ];
  const nothingHappened = (props: ReturnType<typeof renderChooser>["props"]) => {
    for (const fn of [props.onSwitch, props.onResume, props.onNewSession, props.onCloseTab, props.onRenameTab, props.onLeave]) {
      expect(fn).not.toHaveBeenCalled();
    }
  };

  it("K03: a filter matching nothing keeps the chooser drawn, and Esc still leaves", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.change(filter, { target: { value: "zzz" } });
    expect(container.querySelector(".chooser-empty")?.textContent).toBe("nothing matches");
    fireEvent.keyDown(filter, { key: "Escape" });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(props.onLeave).toHaveBeenCalled();
  });

  it("K03: with no row under the cursor the mode line is empty, and Enter, x, Ctrl+r and Shift+Tab choose nothing", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.change(filter, { target: { value: "zzz" } });
    fireEvent.keyDown(filter, { key: "Enter" }); // back to the list; the filter and its empty result stay
    expect(container.querySelector(".chooser-empty")).not.toBeNull();
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("");
    for (const init of [{ key: "Enter" }, { key: "x" }, { key: "r", ctrlKey: true }, { key: "Tab", shiftKey: true }]) {
      fireEvent.keyDown(root, init);
    }
    nothingHappened(props);
    expect(props.onCycleMode).not.toHaveBeenCalled();
    expect(props.onCycleTabMode).not.toHaveBeenCalled();
    expect(container.querySelector(".chooser-rename")).toBeNull();
  });

  it("K03: clearing the filter after an empty result leaves a row under the cursor, never none", () => {
    const { root, container } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.change(filter, { target: { value: "zzz" } });
    fireEvent.keyDown(filter, { key: "Enter" });
    fireEvent.keyDown(root, { key: "j" }); // on an empty list: used to leave the cursor at -1
    fireEvent.keyDown(root, { key: "ArrowDown" });
    fireEvent.keyDown(root, { key: "/" });
    fireEvent.keyDown(container.querySelector(".chooser-filter")!, { key: "Escape" }); // clears the filter
    expect(currentRow(container)).toContain("New session");
  });

  it("↓ ↑ Ctrl+n Ctrl+p move the list, from the list and from the filter (R12)", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "ArrowDown" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onSwitch).toHaveBeenLastCalledWith(1);
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.keyDown(filter, { key: "n", ctrlKey: true }); // tab 1 -> tab 2
    fireEvent.keyDown(filter, { key: "ArrowUp" }); // -> tab 1
    fireEvent.keyDown(filter, { key: "ArrowDown" }); // -> tab 2, still in the filter
    expect(document.activeElement).toBe(filter);
    fireEvent.keyDown(filter, { key: "Enter" }); // leaves the filter
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onSwitch).toHaveBeenLastCalledWith(2);
  });

  it("each of the four keys moves one row and stops at both ends, from the list", () => {
    const { root, container } = renderChooser(); // rows: New session, tab 1, tab 2, held one, free one
    const press = (init: Record<string, unknown>) => fireEvent.keyDown(root, init);
    press({ key: "ArrowUp" });
    press({ key: "p", ctrlKey: true });
    expect(currentRow(container), "already on the first row").toContain("New session");
    press({ key: "n", ctrlKey: true });
    expect(currentRow(container)).toContain("1 fix-parser");
    press({ key: "ArrowDown" });
    expect(currentRow(container)).toContain("2 legacy");
    press({ key: "ArrowDown" });
    press({ key: "ArrowDown" });
    expect(currentRow(container)).toContain("free one");
    press({ key: "n", ctrlKey: true });
    press({ key: "ArrowDown" });
    expect(currentRow(container), "already on the last row").toContain("free one");
    press({ key: "ArrowUp" });
    expect(currentRow(container)).toContain("held one");
    press({ key: "p", ctrlKey: true });
    expect(currentRow(container)).toContain("2 legacy");
  });

  it("in the filter box the four keys are claimed and move the list, while j, k, n and p stay typed text", () => {
    const { root, container } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    for (const key of ["j", "k", "n", "p"]) {
      expect(fireEvent.keyDown(filter, { key }), `${key} is left to the box`).toBe(true);
    }
    expect(currentRow(container)).toContain("New session");
    for (const init of FOUR) expect(fireEvent.keyDown(filter, init), JSON.stringify(init)).toBe(false);
    // down, up, down, up: back where it began.
    expect(currentRow(container)).toContain("New session");
  });

  it("the four keys never reach the panel under the chooser, from the list or from the filter", () => {
    const outer = vi.fn();
    const { props, rerender } = renderChooser();
    rerender(
      <div onKeyDown={(e) => outer(e.key)}>
        <Chooser {...props} />
      </div>,
    );
    const el = () => document.querySelector<HTMLElement>(".chooser")!;
    for (const init of FOUR) expect(fireEvent.keyDown(el(), init), JSON.stringify(init)).toBe(false);
    fireEvent.keyDown(el(), { key: "/" });
    const filter = document.querySelector<HTMLInputElement>(".chooser-filter")!;
    for (const init of FOUR) fireEvent.keyDown(filter, init);
    expect(outer).not.toHaveBeenCalled();
  });

  it("while a tab is being renamed the four keys are the text field's own", () => {
    const { root, container } = renderChooser({ active: 1 });
    fireEvent.keyDown(root, { key: "r", ctrlKey: true });
    const field = container.querySelector<HTMLInputElement>(".chooser-rename")!;
    for (const init of FOUR) expect(fireEvent.keyDown(field, init), JSON.stringify(init)).toBe(true);
    // The rename field lives in the row under the cursor: it is still there, so the cursor never moved.
    expect(container.querySelector(".chooser-row.current .chooser-rename")).toBe(field);
  });

  it("a bare n or p does nothing on the list: only Ctrl+n and Ctrl+p move it", () => {
    const { root, container, props } = renderChooser({ active: 1 });
    expect(fireEvent.keyDown(root, { key: "n" }), "n is not claimed").toBe(true);
    expect(fireEvent.keyDown(root, { key: "p" }), "p is not claimed").toBe(true);
    expect(currentRow(container)).toContain("1 fix-parser");
    nothingHappened(props);
  });

  it("g then a key that is not g does nothing (R1)", () => {
    const { root, props } = renderChooser();
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onCloseTab).not.toHaveBeenCalled();
  });

  it("g then anything but g ends the wait and runs nothing: Enter, Esc, q, /, x, G, ↓ and Ctrl+n", () => {
    for (const init of [
      { key: "Enter" },
      { key: "Escape" },
      { key: "q" },
      { key: "/" },
      { key: "x" },
      { key: "G" },
      { key: "ArrowDown" },
      { key: "n", ctrlKey: true },
    ]) {
      const { root, container, props } = renderChooser({ active: 1 });
      fireEvent.keyDown(root, { key: "g" });
      expect(fireEvent.keyDown(root, init), `${JSON.stringify(init)} is claimed`).toBe(false);
      expect(currentRow(container), JSON.stringify(init)).toContain("1 fix-parser");
      expect(container.querySelector(".chooser-filter"), JSON.stringify(init)).toBeNull();
      nothingHappened(props);
      // The wait is over, not stuck: the next key is an ordinary one again.
      fireEvent.keyDown(root, { key: "j" });
      expect(currentRow(container), JSON.stringify(init)).toContain("2 legacy");
      cleanup();
    }
  });

  it("g then g still goes to the first row, with a bare modifier between them or not", () => {
    const { root, container } = renderChooser({ active: 2 });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow(container)).toContain("New session");
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "g" });
    for (const key of ["Shift", "Control", "Alt", "Meta"]) fireEvent.keyDown(root, { key });
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow(container)).toContain("New session");
  });

  it("g, a bare modifier, then a key that is not g still cancels (a bare modifier is not the next key)", () => {
    const { root, container, props } = renderChooser({ active: 1 });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "Shift" });
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onCloseTab).not.toHaveBeenCalled();
    expect(currentRow(container)).toContain("1 fix-parser");
  });
});

/** rc.3 minors (K01's cancel routes, for the chooser's own `g`): a lone `g` waits for its next key with
 *  no timeout, so any route that ends a pending key elsewhere in the window has to end this one too --
 *  Shift+Tab (the chooser sees it as a key of its own) and the window's cancel routes that reach it as
 *  `dropKeysRequest` (a focus round trip, `arrive`, a HINT, a tab switch, an overlay). Left waiting, the
 *  next `j` after either was swallowed as the cancel of a `g` nobody meant any more. */
describe("Chooser: a waiting g ends on the window's cancel routes (K01)", () => {
  const currentRow = (c: HTMLElement) => c.querySelector(".chooser-row.current")?.textContent ?? null;

  it("Shift+Tab ends it: g, Shift+Tab, then j moves the cursor", () => {
    const { root, container } = renderChooser({ active: 1 });
    expect(currentRow(container)).toContain("1 fix-parser");
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    fireEvent.keyDown(root, { key: "j" });
    expect(currentRow(container)).toContain("2 legacy");
  });

  it("Shift+Tab ends it: g, Shift+Tab, g is a fresh wait, not the second half of gg", () => {
    const { root, container } = renderChooser({ active: 2 });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow(container), "no gg happened").toContain("2 legacy");
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow(container), "a real gg still does").toContain("New session");
  });

  it("a bump of dropKeysRequest ends it: g, a cancel route, then j moves the cursor", () => {
    const { root, container, props, rerender } = renderChooser({ active: 1, dropKeysRequest: 0 });
    fireEvent.keyDown(root, { key: "g" });
    rerender(<Chooser {...props} dropKeysRequest={1} />);
    fireEvent.keyDown(root, { key: "j" });
    expect(currentRow(container)).toContain("2 legacy");
  });

  it("without a bump the wait stands: g, a re-render, then j is still the swallowed cancel key", () => {
    const { root, container, props, rerender } = renderChooser({ active: 1, dropKeysRequest: 3 });
    fireEvent.keyDown(root, { key: "g" });
    rerender(<Chooser {...props} dropKeysRequest={3} />);
    fireEvent.keyDown(root, { key: "j" });
    expect(currentRow(container)).toContain("1 fix-parser");
  });
});
