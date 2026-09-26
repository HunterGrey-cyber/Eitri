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
  launch: false,
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
    backend: "sidecar" as const,
    projectDir: "/home/user/src/neovibe",
    newTabChord: "Ctrl+b c",
    focusRequest: 0,
    onSwitch: vi.fn(),
    onResume: vi.fn(),
    onNewSession: vi.fn(),
    onCloseTab: vi.fn(),
    onRenameTab: vi.fn(),
    onCycleMode: vi.fn(),
    onLeave: vi.fn(),
    ...over,
  };
  const view = render(<Chooser {...props} />);
  const root = view.container.querySelector<HTMLElement>(".chooser")!;
  return { props, root, ...view };
}

describe("Chooser", () => {
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
  it("Esc and q leave, saying whether this was the launch chooser", () => {
    const first = renderChooser({ envelope: { ...ENVELOPE, launch: true } });
    fireEvent.keyDown(first.root, { key: "Escape" });
    expect(first.props.onLeave).toHaveBeenCalledWith(true);
    cleanup();
    const second = renderChooser();
    fireEvent.keyDown(second.root, { key: "q" });
    expect(second.props.onLeave).toHaveBeenCalledWith(false);
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

  it("a record row's line 2 names the sidecar and its short id, or is untitled with neither name nor title", () => {
    const { container } = renderChooser({
      envelope: {
        launch: false,
        open: [],
        records: [
          { providerSessionId: "aaaa1111bbbb", name: null, title: null, createdAt: "1", updatedAt: "2", heldElsewhere: false },
        ],
      },
    });
    const row = container.querySelectorAll(".chooser-row")[1]; // New session, then the record
    expect(row.querySelector(".chooser-line2")!.textContent).toBe("sidecar · aaaa1111");
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

  it("the mode line reads Resume/Start on New session and a record, and a fixed pill on an open tab", () => {
    const { root, container } = renderChooser({ defaultMode: "auto" });
    // cursor starts on New session
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("Start in ⏵⏵ auto mode on (shift+tab to cycle)");
    fireEvent.keyDown(root, { key: "j" }); // tab 1, mode auto
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("Tab 1 runs in ⏵⏵ auto mode on · fixed");
    fireEvent.keyDown(root, { key: "j" }); // tab 2, mode bypass
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("Tab 2 runs in ⏵⏵ bypass mode on · fixed");
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" }); // a record
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("Resume in ⏵⏵ auto mode on (shift+tab to cycle)");
  });

  it("Shift+Tab cycles the resume mode on New session and a record, and flashes fixed on an open tab", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true }); // on New session
    expect(props.onCycleMode).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(root, { key: "j" }); // tab 1
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(props.onCycleMode).toHaveBeenCalledTimes(1); // not called again
    expect(container.querySelector(".chooser-mode-line")!.textContent).toBe("mode is fixed for this session");
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
});
