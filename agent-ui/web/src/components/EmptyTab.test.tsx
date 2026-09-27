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
    // An unrecognised failure (spec §10.2): no headline/remedy block, exactly as before this task.
    expect(container.querySelector(".row-problem")).toBeNull();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "r" });
    expect(props.onReset).toHaveBeenCalled();
  });
  // Spec §10.2/§10.3 (P11): a recognised startup failure draws a headline and remedy above the
  // raw text, which stays -- "never hide the evidence".
  it("classifies a recognised startup failure into a plain headline and remedy above the raw text", () => {
    const { container, getByText } = renderEmpty({
      tab: { ...TAB, state: "failed" },
      failure: 'could not determine the installed claude CLI version via "/no/such/claude" (spawn error: ENOENT)',
    });
    // The raw text is still there, unchanged.
    getByText(/could not determine the installed claude CLI version/);
    const problem = container.querySelector(".row-problem")!;
    expect(problem.querySelector("strong")!.textContent).toBe("Claude Code (claude) was not found.");
    expect(problem.querySelector(".row-problem-remedy")!.textContent).toBe(
      "Install it and make sure claude runs in a terminal, then press r.",
    );
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

    it("the Mode item still runs on Enter, and its key column now reads ⇧Tab (V1 S2)", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      // HELLO above has records, so the table is new/resume/sessions/mode/keys -- "mode" is index 3.
      const items = Array.from(root.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
      expect(items[3].querySelector(".dash-key")!.textContent).toBe("⇧Tab");
      for (let i = 0; i < 3; i++) fireEvent.keyDown(root, { key: "j" });
      fireEvent.keyDown(root, { key: "Enter" });
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
          // A real Space still does, so the guard is not simply swallowing Space -- on its own, after
          // a pause: in the middle of typing the leader is refused (the v1-ui GUI pass, below).
          act(() => vi.advanceTimersByTime(300));
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
          expect(container.querySelector(".which-key-box")).not.toBeNull();
        } finally {
          vi.useRealTimers();
        }
      });

      /** The v1-ui GUI pass (2026-09-27): "set up my" typed onto this dashboard ran `<leader>m` and
       *  flipped the stored mode to bypass -- the thing S2 took the bare `m` away to stop. The leader
       *  here starts a sequence only on a key that stands alone or ends a quick motion. */
      it("t Space m at 80 ms a key: <leader>m never runs, and the band is told why", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const onFlash = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction, onFlash });
          const { props, container } = rendered;
          const root = toBrowse(rendered, props);
          act(() => vi.advanceTimersByTime(1000));
          for (const key of ["t", " ", "m"]) {
            fireEvent.keyDown(root, { key });
            act(() => vi.advanceTimersByTime(80));
          }
          act(() => vi.advanceTimersByTime(1000));
          expect(onPanelAction).not.toHaveBeenCalled();
          expect(props.onCycleMode).not.toHaveBeenCalled();
          expect(container.querySelector(".which-key-box")).toBeNull();
          expect(onFlash).toHaveBeenCalledWith("Space starts a sequence only on its own — i or Ctrl+j to type");
        } finally {
          vi.useRealTimers();
        }
      });

      it("j then Space m at 80 ms a key is a quick motion: <leader>m runs", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          act(() => vi.advanceTimersByTime(1000));
          for (const key of ["j", " ", "m"]) {
            fireEvent.keyDown(root, { key });
            act(() => vi.advanceTimersByTime(80));
          }
          expect(onPanelAction).toHaveBeenCalledWith(expect.objectContaining({ action: "mode.cycle" }));
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

describe("v1 S2: the dashboard's bare m is removed (spec §2.4)", () => {
  it("m in the menu posts nothing and changes no mode", () => {
    const rendered = renderEmpty();
    const { props } = rendered;
    const root = toBrowse(rendered, props);
    fireEvent.keyDown(root, { key: "m" });
    expect(props.onCycleMode).not.toHaveBeenCalled();
  });

  it("typing 'make' changes nothing but the cursor (the k in it)", () => {
    const rendered = renderEmpty();
    const { props } = rendered;
    const root = toBrowse(rendered, props);
    const current = () => root.querySelector<HTMLElement>('[aria-current="true"]');
    const items = () => Array.from(root.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    // Start away from the top so `k` (cursor up) has something to do, and is not itself clamped
    // into looking like a no-op the way it would be at index 0.
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    expect(items().indexOf(current()!)).toBe(2);
    for (const key of ["m", "a", "k", "e"]) fireEvent.keyDown(root, { key });
    // m, a and e are unbound letters and do nothing; only k -- the cursor-up key -- moved anything.
    expect(items().indexOf(current()!)).toBe(1);
    expect(props.onCycleMode).not.toHaveBeenCalled();
    expect(props.onResume).not.toHaveBeenCalled();
  });
});

describe("v1 P11: the starting screen's checkout hint is delayed 10s (F19, spec §10.2)", () => {
  it("shows no 'fresh Verdandi checkout' text before 10s", () => {
    vi.useFakeTimers();
    try {
      const { container, getByText } = renderEmpty({ tab: { ...TAB, state: "starting" } });
      getByText(/Starting the agent backend/);
      expect(container.textContent).not.toContain("Verdandi checkout");
      act(() => vi.advanceTimersByTime(9_999));
      expect(container.textContent).not.toContain("Verdandi checkout");
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows the 'still starting' line once 10s have passed", () => {
    vi.useFakeTimers();
    try {
      const { getByText } = renderEmpty({ tab: { ...TAB, state: "starting" } });
      act(() => vi.advanceTimersByTime(10_000));
      getByText("still starting — a first start from a Verdandi checkout builds the sidecar");
    } finally {
      vi.useRealTimers();
    }
  });

  it("never shows it once the tab is no longer starting", () => {
    vi.useFakeTimers();
    try {
      const rendered = renderEmpty({ tab: { ...TAB, state: "starting" } });
      const { container, props } = rendered;
      act(() => vi.advanceTimersByTime(10_000));
      expect(container.textContent).toContain("Verdandi checkout");
      rendered.rerender(<EmptyTab {...props} tab={{ ...TAB, state: "not_started" }} />);
      expect(container.textContent).not.toContain("Verdandi checkout");
      expect(container.querySelector(".connecting")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

/* Task 3 (v1 v1-ui plan, spec §3.1/§3.5): `App.tsx` bumps `navKeyRequest` only for a `nav_key` it
   has not already answered with `nav_fallthrough` itself (an overlay it alone knows about, `?`, a
   y/n) -- this screen decides the rest: a starting/failed tab, or its own menu/composer mode not
   matching the direction, both report back through `onNavFallthrough` rather than transitioning. */
describe("EmptyTab: C1's navKeyRequest (v1 spec §3.1, §3.5)", () => {
  it("menu nav_key down opens the composer", () => {
    const rendered = renderEmpty();
    toBrowse(rendered, rendered.props);
    expect(rendered.container.querySelector("textarea")).toBeNull();
    rendered.rerender(<EmptyTab {...rendered.props} arriveRequest={1} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(rendered.container.querySelector("textarea")).not.toBeNull();
  });

  it("composer nav_key up goes back to the menu, root focused", () => {
    const rendered = renderEmpty();
    expect(rendered.container.querySelector("textarea")).not.toBeNull();
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "up" }} />);
    expect(rendered.container.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(rendered.container.querySelector(".empty-tab"));
  });

  it("a starting tab falls through rather than transitioning", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "starting" }, onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(onNavFallthrough).toHaveBeenCalledWith("down");
    // The starting tab's own box stays live regardless (C1: it queues behind the connect) --
    // falling through changes nothing about it either way.
    expect(rendered.container.querySelector("textarea")).not.toBeNull();
  });

  it("a failed tab falls through rather than transitioning", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "boom", onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(onNavFallthrough).toHaveBeenCalledWith("down");
  });

  // Fix round 1 (reviewer finding): a starting tab's composer is live ("C1: it queues behind the
  // connect", this file's own `onKeyDown` comment), so leaving an already-open INPUT via `Ctrl+k`
  // must behave the same way `Esc` already does there -- not fall through and hand the keys off this
  // screen entirely, contradicting `Escape`'s own handling two tests up.
  it("a starting tab already in INPUT leaves it on nav_key up, like Esc -- the composer stays live", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "starting" }, onNavFallthrough });
    expect(rendered.container.querySelector("textarea")).not.toBeNull(); // mounts INPUT by default.
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "up" }} />);
    expect(onNavFallthrough).not.toHaveBeenCalled();
    expect(rendered.container.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(rendered.container.querySelector(".empty-tab"));
  });

  // A `failed` tab's own `Escape` handling is never exempted the same way (its composer is
  // `disabled`), so `nav_key up` must not be either -- still a plain fallthrough.
  it("a failed tab's nav_key up still falls through, matching Esc's own !failed guard", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "boom", onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "up" }} />);
    expect(onNavFallthrough).toHaveBeenCalledWith("up");
  });

  it("a stale direction (already in the target mode) falls through", () => {
    const onNavFallthrough = vi.fn();
    // Mounts in the composer (INPUT) by default -- "down" only applies from the menu (BROWSE).
    const rendered = renderEmpty({ onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(onNavFallthrough).toHaveBeenCalledWith("down");
    expect(rendered.container.querySelector("textarea")).not.toBeNull();
  });

  it("an overlay open (props.overlayOpen) falls through rather than stealing its keys", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ overlayOpen: true, onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "up" }} />);
    expect(onNavFallthrough).toHaveBeenCalledWith("up");
  });

  it("a request counted before this screen mounted is not one (requests are edges)", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ navKeyRequest: { seq: 3, direction: "down" }, onNavFallthrough });
    expect(onNavFallthrough).not.toHaveBeenCalled();
    expect(rendered.container.querySelector("textarea")).not.toBeNull(); // untouched default (INPUT)
  });

  it("the same seq twice is one request, not two", () => {
    const onNavFallthrough = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "starting" }, onNavFallthrough });
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(onNavFallthrough).toHaveBeenCalledTimes(1);
    rendered.rerender(<EmptyTab {...rendered.props} navKeyRequest={{ seq: 1, direction: "down" }} />);
    expect(onNavFallthrough).toHaveBeenCalledTimes(1);
  });
});
