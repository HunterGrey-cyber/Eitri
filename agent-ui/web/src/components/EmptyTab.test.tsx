// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import type { RenderResult } from "@testing-library/react";
import { EmptyTab } from "./EmptyTab";
import type { EmptyTabProps } from "./EmptyTab";
import type { Hello, ResumableSession, TabInfo } from "../types";
import { TABLE } from "../testFixtures";
import { WHICH_KEY_DELAY_MS } from "../leader";

afterEach(cleanup);

const session = (n: number): ResumableSession => ({
  provider: "claude", providerSessionId: `id-${n}-0000000000`, createdAt: "1", updatedAt: "2", title: `about ${n}`, name: null,
});
const HELLO: Hello = {
  backend: "sidecar", projectDir: "/p", permissionModes: ["auto", "bypass"],
  resumableSessions: Array.from({ length: 10 }, (_, i) => session(i)), expectedVerdandiRevision: "28a5e4c",
  account: null,
};
const TAB: TabInfo = {
  id: 1, number: 1, label: "1 new", name: null, state: "not_started", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null,
};

function renderEmpty(over: Partial<EmptyTabProps> = {}) {
  const props: EmptyTabProps = {
    hello: HELLO, tab: TAB, handoff: null, failure: null, paneFocused: true, focusRequest: 0, arriveRequest: 0,
    keysRequest: 0, overlayOpen: false, restoredDraft: null,
    onSend: vi.fn(), onResume: vi.fn(), onCycleMode: vi.fn(), onReset: vi.fn(), onHint: vi.fn(), ...over,
  };
  return { props, ...render(<EmptyTab {...props} />) };
}

/** Switches to BROWSE without going through the composer's own `Shift+Tab`/`i` routes -- an
 *  `arrive` bump (spec §8) is the shape a real arrival takes and also resets the dashboard cursor
 *  to `"new"` (Task 12), so it doubles as this file's way to get keys to `.empty-tab`'s own
 *  `onKeyDown` rather than the textarea's. */
function toBrowse(rendered: RenderResult, props: EmptyTabProps): HTMLElement {
  rendered.rerender(<EmptyTab {...props} arriveRequest={1} />);
  return rendered.container.querySelector(".empty-tab")!;
}

describe("EmptyTab: requests are edges, not levels (GUI pass 2026-09-26, r2-gui)", () => {
  it("an arrival counted before it mounted is not one: it mounts where it was landed", () => {
    // The window's counters outlive any one tab: a new tab mounts with whatever `arrive` count the
    // window reached long ago, and that count is not a request to this tab.
    const { container } = renderEmpty({ arriveRequest: 3, focusRequest: 2 });
    expect(container.querySelector("textarea")).not.toBeNull();
  });
  it("mounts in BROWSE when the window landed it there", () => {
    const { container } = renderEmpty({ landing: "browse" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-browse-hint")).not.toBeNull();
  });
  it("lands a switch to another empty tab where the window landed it, without remounting", () => {
    const rendered = renderEmpty();
    expect(rendered.container.querySelector("textarea")).not.toBeNull();
    rendered.rerender(<EmptyTab {...rendered.props} tab={{ ...TAB, id: 2, number: 2, label: "2 new" }} landing="browse" />);
    expect(rendered.container.querySelector("textarea")).toBeNull();
  });
  it("tells the window its mode, for the band under it", () => {
    const onModeChange = vi.fn();
    const rendered = renderEmpty({ onModeChange });
    expect(onModeChange).toHaveBeenLastCalledWith("input");
    toBrowse(rendered, { ...rendered.props, onModeChange });
    expect(onModeChange).toHaveBeenLastCalledWith("browse");
  });
});

describe("EmptyTab (F3)", () => {
  it("is Claude Code's fresh prompt: a live composer, with the dashboard drawn under it", () => {
    // The mode pill moved into the footer `App.tsx` draws (Task 9); see `App.test.tsx`'s
    // "draws the empty tab's close prompt in the footer" for where it is covered now.
    const { container } = renderEmpty();
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    // Panel round 2 (Task 12): the eight resume rows are gone, replaced by the dashboard's own
    // five (`new`, `resume`, `sessions`, `mode`, `keys`) items -- `HELLO` above has ten records, so
    // this also pins that the dashboard never draws one row per record the way the old screen did.
    expect(container.querySelectorAll('[data-nav-stop="resume"]').length).toBe(0);
    expect(container.querySelectorAll('[data-nav-stop="dash"]').length).toBe(5);
    expect(container.textContent).not.toContain("about 8");
    expect(container.querySelector(".dashboard")).not.toBeNull();
  });
  it("sends what is typed, which starts the session lazily in Rust", () => {
    const { container, props } = renderEmpty();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "fix the parser" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(props.onSend).toHaveBeenCalledWith("fix the parser");
  });
  // Wave 4 Task 1: Shift+Tab used to be claimed by this screen's own `onKeyDown` directly; it is
  // now caught everywhere in the chat by App.tsx's document-capture router (`modeKey.ts`, tested in
  // App.test.tsx's "Shift+Tab anywhere in the chat" describe block), so that test moved there.
  it("says it is starting while the session connects, with the box live, queueing", () => {
    const onQueue = vi.fn();
    const { container, getByText } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    getByText(/Starting the agent backend/);
    const box = container.querySelector("textarea")!;
    expect(box.disabled).toBe(false);
    fireEvent.change(box, { target: { value: "queue this" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onQueue).toHaveBeenCalledWith("queue this");
  });
  /** Fix round 1 (reviewer finding, blocking): a starting tab has no live turn to send-now to, so
   *  Ctrl+Enter used to fall through to `Composer`'s default no-op `onSendNow` while its `submit()`
   *  cleared the box anyway -- the typed text vanished with no send, no queue and no restore. */
  it("queues on Ctrl+Enter too, instead of silently discarding the draft (fix round 1)", () => {
    const onQueue = vi.fn();
    const { container } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "important text" } });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(onQueue).toHaveBeenCalledWith("important text");
  });
  it("does not queue a blank entry when Ctrl+Enter is pressed on an empty box", () => {
    const onQueue = vi.fn();
    const { container } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    const box = container.querySelector("textarea")!;
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(onQueue).not.toHaveBeenCalled();
  });
  it("shows why a failed tab failed, and r starts it over, with no dashboard", () => {
    const { container, getByText, props } = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "the gate refused 2.1.999" });
    getByText("the gate refused 2.1.999");
    expect(container.querySelector(".dashboard")).toBeNull();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "r" });
    expect(props.onReset).toHaveBeenCalled();
  });
  it("draws no dashboard while starting", () => {
    const { container } = renderEmpty({ tab: { ...TAB, state: "starting" } });
    expect(container.querySelector(".dashboard")).toBeNull();
  });
  it("shows the handoff card of a conversation handed to a terminal (ruling 13)", () => {
    const { getByText } = renderEmpty({
      handoff: { command: "cd /p && claude --resume abc", cwd: "/p", providerSessionId: "abc" },
    });
    getByText("cd /p && claude --resume abc");
  });

  describe("BROWSE: j/k move the dashboard cursor, letters and Enter run items", () => {
    it("arrive lands BROWSE with the cursor on New session, closing the fresh prompt's default composer", () => {
      const rendered = renderEmpty();
      const { props, container } = rendered;
      expect(container.querySelector("textarea")).not.toBeNull();
      const root = toBrowse(rendered, props);
      expect(container.querySelector("textarea")).toBeNull();
      const items = Array.from(root.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
      expect(items[0].getAttribute("aria-current")).toBe("true");
    });

    it("j/k move the cursor, clamped at both ends", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      const current = () => root.querySelector<HTMLElement>('[aria-current="true"]');
      const items = () => Array.from(root.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
      expect(items().indexOf(current()!)).toBe(0);
      fireEvent.keyDown(root, { key: "j" });
      expect(items().indexOf(current()!)).toBe(1);
      fireEvent.keyDown(root, { key: "k" });
      expect(items().indexOf(current()!)).toBe(0);
      // Clamped, not wrapped: holding k at the top stays put.
      fireEvent.keyDown(root, { key: "k" });
      expect(items().indexOf(current()!)).toBe(0);
      // HELLO above has records, so the table is new/resume/sessions/mode/keys -- 5 items, last index 4.
      for (let i = 0; i < 10; i++) fireEvent.keyDown(root, { key: "j" });
      expect(items().indexOf(current()!)).toBe(4);
    });

    it("Enter runs the item under the cursor", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "j" }); // -> resume
      fireEvent.keyDown(root, { key: "Enter" });
      expect(props.onResume).toHaveBeenCalledWith("id-0-0000000000");
    });

    it("i opens the composer in INPUT", () => {
      const rendered = renderEmpty();
      const { props, container } = rendered;
      const root = toBrowse(rendered, props);
      expect(container.querySelector("textarea")).toBeNull();
      fireEvent.keyDown(root, { key: "i" });
      expect(container.querySelector("textarea")).not.toBeNull();
    });

    it("r resumes the newest record directly, regardless of the cursor", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "r" });
      expect(props.onResume).toHaveBeenCalledWith("id-0-0000000000");
    });

    it("r does nothing when there is no record to resume", () => {
      const rendered = renderEmpty({ hello: { ...HELLO, resumableSessions: [] } });
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "r" });
      expect(props.onResume).not.toHaveBeenCalled();
    });

    it("w opens the chooser (spec §7: All sessions)", () => {
      const onChooseSessions = vi.fn();
      const rendered = renderEmpty({ onChooseSessions });
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "w" });
      expect(onChooseSessions).toHaveBeenCalledTimes(1);
    });

    it("m cycles the mode, same as Shift+Tab", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "m" });
      expect(props.onCycleMode).toHaveBeenCalledTimes(1);
    });

    it("? opens the keymap overlay", () => {
      const onOpenKeymap = vi.fn();
      const rendered = renderEmpty({ onOpenKeymap });
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "?" });
      expect(onOpenKeymap).toHaveBeenCalledTimes(1);
    });

    it("Space is left for the leader system, not swallowed by a dashboard item", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      const event = fireEvent.keyDown(root, { key: " " });
      // `fireEvent` returns `false` only when `preventDefault()` was called; this component must
      // never call it for Space, or the leader engine above it never sees the key.
      expect(event).toBe(true);
      expect(props.onCycleMode).not.toHaveBeenCalled();
      expect(props.onResume).not.toHaveBeenCalled();
    });

    it("a click on a dashboard item runs it, same as its key", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      const items = Array.from(root.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
      fireEvent.click(items[1]); // "resume"
      expect(props.onResume).toHaveBeenCalledWith("id-0-0000000000");
    });

    /** Fix round 1 (panel round 2 plan Task 12+13, reviewer finding, blocking): the leader engine
     *  reachable here at all -- this reproduces the exact scenario the finding describes (mounting
     *  an EmptyTab, arriving, firing Space) and checks the box that finding said stayed `null`. */
    describe("the leader engine (fix round 1)", () => {
      it("shows the which-key box 200ms after Space, once a real table is supplied", () => {
        vi.useFakeTimers();
        try {
          const rendered = renderEmpty({ panelTable: TABLE });
          const { props, container } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
          expect(container.querySelector(".which-key-box")).toBeNull();
          act(() => vi.advanceTimersByTime(1));
          const box = container.querySelector(".which-key-box")!;
          expect(box).not.toBeNull();
          expect(box.querySelector(".wk-title")!.textContent).toBe("Space");
        } finally {
          vi.useRealTimers();
        }
      });

      /** Fix round 2: an input method's Space never starts a sequence here, whether it arrives
       *  with `isComposing` or only as WebKit's `keyCode` 229 -- the same `isImeKey` guard App.tsx
       *  uses for its own leader engine. */
      it("an IME Space (isComposing, or keyCode 229) starts no sequence", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props, container } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: " ", isComposing: true });
          fireEvent.keyDown(root, { key: " ", keyCode: 229 });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
          expect(container.querySelector(".which-key-box")).toBeNull();
          // Nothing pending: `b` then `d` alone do not complete `Space b d`.
          fireEvent.keyDown(root, { key: "b" });
          fireEvent.keyDown(root, { key: "d" });
          expect(onPanelAction).not.toHaveBeenCalledWith(expect.objectContaining({ action: "tab.close" }));
          // A real Space still does, so the guard is not simply swallowing Space.
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
          expect(container.querySelector(".which-key-box")).not.toBeNull();
        } finally {
          vi.useRealTimers();
        }
      });

      it("Space b d runs the table's tab.close binding through onPanelAction, never a dashboard item", () => {
        const onPanelAction = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
        const { props, container } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        fireEvent.keyDown(root, { key: "b" });
        fireEvent.keyDown(root, { key: "d" });
        expect(onPanelAction).toHaveBeenCalledWith({ keys: ["<leader>", "b", "d"], action: "tab.close", desc: "close tab", source: "default" });
        expect(props.onCycleMode).not.toHaveBeenCalled();
        expect(props.onResume).not.toHaveBeenCalled();
        expect(container.querySelector(".which-key-box")).toBeNull();
      });

      /** The whole-branch review: `[b`/`]b` (spec §4) reached nothing here, since `startSequence`
       *  leaves `[`/`]` to `resolveKey`, which this screen does not run. */
      it("[ b and ] b run the table's tab.prev / tab.next, and a stray [ is dropped", () => {
        const onPanelAction = vi.fn();
        const table = {
          ...TABLE,
          bindings: [...TABLE.bindings, { keys: ["]", "b"], action: "tab.next" as const, desc: "tab.next", source: "default" as const }],
        };
        const rendered = renderEmpty({ panelTable: table, onPanelAction });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: "[" });
        fireEvent.keyDown(root, { key: "b" });
        expect(onPanelAction).toHaveBeenLastCalledWith(expect.objectContaining({ keys: ["[", "b"], action: "tab.prev" }));
        fireEvent.keyDown(root, { key: "]" });
        fireEvent.keyDown(root, { key: "b" });
        expect(onPanelAction).toHaveBeenLastCalledWith(expect.objectContaining({ keys: ["]", "b"], action: "tab.next" }));
        expect(onPanelAction).toHaveBeenCalledTimes(2);
        // `[` then a key with no pair: the prefix is dropped, and a later lone `b` does nothing.
        fireEvent.keyDown(root, { key: "[" });
        fireEvent.keyDown(root, { key: "x" });
        fireEvent.keyDown(root, { key: "b" });
        expect(onPanelAction).toHaveBeenCalledTimes(2);
      });

      /** Spec §2.4's cancel list: the keys leaving this pane ends a pending sequence here too. */
      it("losing pane focus cancels a pending sequence", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props, container } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
          expect(container.querySelector(".which-key-box")).not.toBeNull();
          rendered.rerender(<EmptyTab {...props} arriveRequest={1} paneFocused={false} />);
          expect(container.querySelector(".which-key-box")).toBeNull();
          rendered.rerender(<EmptyTab {...props} arriveRequest={1} paneFocused={true} />);
          fireEvent.keyDown(root, { key: "b" });
          fireEvent.keyDown(root, { key: "d" });
          expect(onPanelAction).not.toHaveBeenCalledWith(expect.objectContaining({ action: "tab.close" }));
        } finally {
          vi.useRealTimers();
        }
      });

      it("a direct table binding (H) runs immediately, ahead of the dashboard's own fixed keys", () => {
        const onPanelAction = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: "H" });
        expect(onPanelAction).toHaveBeenCalledWith({ keys: ["H"], action: "tab.prev", desc: "tab.prev", source: "default" });
      });

      /** `i` is one of the reserved fixed BROWSE keys (Global Constraint #4), so it can never START
       *  a sequence -- but a sequence already pending, mid-typing, swallows any key that does not
       *  continue it (spec §2.4, "any other non-continuing key ends and is swallowed"), `i` included.
       *  This is the exact same rule `App.tsx`'s own describe block pins with `armThenCancel(() =>
       *  press("i"))`; reproduced here so this screen's copy of the engine agrees with it, rather
       *  than leaking into the dashboard's `i` (open the composer) underneath. */
      it("i cancels a pending sequence rather than falling through to the dashboard's own i", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props, container } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
          expect(container.querySelector(".which-key-box")).not.toBeNull();
          fireEvent.keyDown(root, { key: "i" });
          expect(container.querySelector(".which-key-box")).toBeNull();
          expect(container.querySelector("textarea")).toBeNull();
          expect(onPanelAction).not.toHaveBeenCalled();
        } finally {
          vi.useRealTimers();
        }
      });
    });
  });

  it("in INPUT the dashboard's letters type into the composer instead of running an item", () => {
    const { container, props } = renderEmpty(); // starts in INPUT (mode defaults to "input")
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "w" } });
    expect(box.value).toBe("w");
    expect(props.onChooseSessions).toBeUndefined();
  });

  /** Wave 3 Task 1: `arrive` must never leave the keys stranded on `document.body` -- the launch-
   *  chooser investigation's defect 4. Focusing the root happens BEFORE `setMode("browse")`, so the
   *  textarea's `onBlur` reports BROWSE with `relatedTarget` already the root rather than nothing. */
  describe("keysRequest / overlayOpen (wave 3 Task 1)", () => {
    it("arrive with the textarea focused lands the keys on the root, not body", () => {
      const rendered = renderEmpty();
      const { container, props } = rendered;
      expect(document.activeElement).toBe(container.querySelector("textarea"));
      rendered.rerender(<EmptyTab {...props} arriveRequest={1} />);
      expect(container.querySelector("textarea")).toBeNull();
      expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
    });

    it("a keysRequest bump in BROWSE with focus on body brings the keys back to the root", () => {
      const rendered = renderEmpty();
      const { container, props } = rendered;
      const root = toBrowse(rendered, props);
      expect(document.activeElement).toBe(root);
      act(() => (document.activeElement as HTMLElement).blur());
      expect(document.activeElement).toBe(document.body);
      rendered.rerender(<EmptyTab {...props} arriveRequest={1} keysRequest={1} />);
      expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
    });

    it("a keysRequest bump in INPUT focuses the composer's textarea", () => {
      const rendered = renderEmpty();
      const { container, props } = rendered;
      // Moving real DOM focus away from the textarea normally flips `mode` to browse through
      // `Composer`'s own `onBlur` -- to keep testing THIS branch (mode stays "input" while the
      // root does not contain `document.activeElement`) rather than the browse one above, the
      // `relatedTarget` is given `.history-search`'s own class, the one case `onBlur` itself
      // already carves out (`Composer.tsx`'s doc comment: "a blur that lands there must not
      // report BROWSE").
      const outside = document.createElement("button");
      outside.className = "history-search";
      document.body.appendChild(outside);
      act(() => outside.focus());
      expect(document.activeElement).toBe(outside);
      rendered.rerender(<EmptyTab {...props} keysRequest={1} />);
      expect(document.activeElement).toBe(container.querySelector("textarea"));
      document.body.removeChild(outside);
    });

    it("overlayOpen stops focusRequest from opening the composer under the chooser", () => {
      const rendered = renderEmpty();
      const { container, props } = rendered;
      toBrowse(rendered, props);
      expect(container.querySelector("textarea")).toBeNull();
      rendered.rerender(<EmptyTab {...props} arriveRequest={1} overlayOpen={true} focusRequest={1} />);
      expect(container.querySelector("textarea")).toBeNull();
    });
  });

  /** Wave 4 Task 2 (issue 7): "new session界面按esc不能从input区域转换到上面irw那个类似vim初始界面，
   *  没法用hjkl来切换选项" -- Esc in the composer used to be swallowed by the `mode === "input"`
   *  early return with nothing of its own to say about it, stranding the owner in INPUT with no way
   *  back to the dashboard's own j/k/letters. */
  describe("the dashboard's keys (wave 4, Task 2)", () => {
    it("Esc in the composer returns to the menu, keys on the screen's root", () => {
      const onModeChange = vi.fn();
      const { container } = renderEmpty({ onModeChange });
      const textarea = container.querySelector("textarea")!;
      textarea.focus();
      fireEvent.keyDown(textarea, { key: "Escape" });
      expect(container.querySelector("textarea")).toBeNull();
      expect(onModeChange).toHaveBeenLastCalledWith("browse");
      expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
      expect(container.querySelector('.dash-item[aria-current="true"]')?.textContent).toContain("New session");
    });

    it("Esc mid-composition belongs to the input method", () => {
      const { container } = renderEmpty();
      const textarea = container.querySelector("textarea")!;
      textarea.focus();
      fireEvent.keyDown(textarea, { key: "Escape", isComposing: true });
      expect(container.querySelector("textarea")).not.toBeNull();
    });

    it("keeps the draft across Esc and i", () => {
      const { container } = renderEmpty();
      const textarea = container.querySelector("textarea")!;
      fireEvent.change(textarea, { target: { value: "half a thought" } });
      fireEvent.keyDown(textarea, { key: "Escape" });
      fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "i" });
      expect(container.querySelector("textarea")!.value).toBe("half a thought");
    });

    it("j/k and the arrows walk the menu; h/l are claimed and move nothing", () => {
      const rendered = renderEmpty();
      const root = toBrowse(rendered, rendered.props);
      const current = () => rendered.container.querySelector('.dash-item[aria-current="true"]')!.textContent;
      fireEvent.keyDown(root, { key: "j" });
      expect(current()).toContain("Resume last");
      fireEvent.keyDown(root, { key: "ArrowDown" });
      expect(current()).toContain("All sessions");
      fireEvent.keyDown(root, { key: "ArrowUp" });
      expect(current()).toContain("Resume last");
      const l = new KeyboardEvent("keydown", { key: "l", bubbles: true, cancelable: true });
      root.dispatchEvent(l);
      expect(l.defaultPrevented).toBe(true);
      expect(current()).toContain("Resume last");
    });
  });
});
