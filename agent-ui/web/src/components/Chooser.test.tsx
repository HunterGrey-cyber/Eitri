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
    // A lone `g` then another key is not `gg`.
    fireEvent.keyDown(el(), { key: "j" });
    fireEvent.keyDown(el(), { key: "g" });
    fireEvent.keyDown(el(), { key: "j" });
    expect(current()).toContain("2 legacy");
    expect(outer).not.toHaveBeenCalledWith(expect.objectContaining({ key: "g" }));
    expect(outer).not.toHaveBeenCalledWith(expect.objectContaining({ key: "G" }));
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
