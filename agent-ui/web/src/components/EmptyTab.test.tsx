// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import type { RenderResult } from "@testing-library/react";
import { EmptyTab } from "./EmptyTab";
import type { EmptyTabProps } from "./EmptyTab";
import type { Hello, ResumableSession, TabInfo } from "../types";
import { TABLE } from "../testFixtures";
import { WHICH_KEY_DELAY_MS } from "../leader";
import { hintTypingFlash, tableKeyTypingFlash, TYPING_GUARD_MS } from "../typingGuard";

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
  /** v1 polish item 7: "Press r…" was on screen twice (the row and the composer), three times under
   *  a remedy that already says it. Once, whichever way. */
  it("says press r once on a failed tab, with a remedy and without one", () => {
    const withRemedy = renderEmpty({
      tab: { ...TAB, state: "failed" },
      failure: 'could not determine the installed claude CLI version via "/no/such/claude" (spawn error: ENOENT)',
    });
    expect(withRemedy.container.textContent!.match(/press r/gi)).toHaveLength(1);
    cleanup();
    const plain = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "something unrecognised" });
    expect(plain.container.textContent!.match(/press r/gi)).toHaveLength(1);
    expect(plain.container.querySelector(".row-hint")!.textContent).toBe("Press r to start a new session here.");
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

    /** v1 hardening, codex-release-p1 #7 (C1a): this screen had never grown `o`, the live tab's own
     *  exact alias of `i` (`keymap.ts:284-292`) -- only `i` opened the composer here. */
    it("o is an exact alias of i (C1a)", () => {
      const rendered = renderEmpty();
      const { props, container } = rendered;
      const root = toBrowse(rendered, props);
      expect(container.querySelector("textarea")).toBeNull();
      fireEvent.keyDown(root, { key: "o" });
      expect(container.querySelector("textarea")).not.toBeNull();
    });

    /** v1 hardening, codex-release-p1 #7 (C1a): `A` had no branch at all here, so the live tab's
     *  second frozen alias -- open INPUT with the caret forced to the end -- was silently absent on
     *  the very first screen a new user sees. Round trip mirrors `Composer`'s own suite: `i` leaves
     *  the caret at 3 of "hello world", `Escape` returns to BROWSE, and `A` must move it to 11
     *  regardless of where it was left (never simply "kept", which `i` already covers above). */
    it("A opens the composer with the caret forced to the end, not wherever i last left it (C1a)", () => {
      const rendered = renderEmpty();
      const { props, container } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "i" });
      const textarea = container.querySelector("textarea")!;
      fireEvent.change(textarea, { target: { value: "hello world" } });
      textarea.setSelectionRange(3, 3);
      fireEvent.keyUp(textarea, { key: "ArrowLeft" });
      fireEvent.keyDown(textarea, { key: "Escape" });
      expect(container.querySelector("textarea")).toBeNull();
      fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "A", shiftKey: true });
      expect(container.querySelector("textarea")!.selectionStart).toBe("hello world".length);
    });

    /** v1 hardening (the whole-branch review): `r` still resumes the newest record, regardless of
     *  the cursor -- `TYPING_GUARD_MS` later, like `f` and `H`/`L` on this screen, so typed prose
     *  cannot run it (the next tests). */
    it("r resumes the newest record directly, regardless of the cursor, TYPING_GUARD_MS later", () => {
      vi.useFakeTimers();
      try {
        const rendered = renderEmpty();
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: "r" });
        expect(props.onResume).not.toHaveBeenCalled();
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
        expect(props.onResume).toHaveBeenCalledWith("id-0-0000000000");
      } finally {
        vi.useRealTimers();
      }
    });

    it("r does nothing when there is no record to resume", () => {
      vi.useFakeTimers();
      try {
        const rendered = renderEmpty({ hello: { ...HELLO, resumableSessions: [] } });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: "r" });
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
        expect(props.onResume).not.toHaveBeenCalled();
      } finally {
        vi.useRealTimers();
      }
    });

    it("w opens the chooser (spec §7: All sessions), TYPING_GUARD_MS later", () => {
      vi.useFakeTimers();
      try {
        const onChooseSessions = vi.fn();
        const rendered = renderEmpty({ onChooseSessions });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: "w" });
        expect(onChooseSessions).not.toHaveBeenCalled();
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
        expect(onChooseSessions).toHaveBeenCalledTimes(1);
      } finally {
        vi.useRealTimers();
      }
    });

    /** The whole-branch review's reproduction: an arrival lands this dashboard in BROWSE, and "run
     *  the tests" typed at once resumed the newest record into this tab on its own `r`. */
    it('"run" at 80 ms a key resumes nothing, and the band names r', () => {
      vi.useFakeTimers();
      try {
        const onFlash = vi.fn();
        const rendered = renderEmpty({ onFlash });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        for (const key of ["r", "u", "n"]) {
          fireEvent.keyDown(root, { key });
          act(() => vi.advanceTimersByTime(80));
        }
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
        expect(props.onResume).not.toHaveBeenCalled();
        expect(onFlash).toHaveBeenCalledWith(tableKeyTypingFlash("r", "resume the newest session"));
      } finally {
        vi.useRealTimers();
      }
    });

    /** ...and "what's next" opened the modal chooser on its `w`, which then took the rest of the
     *  sentence as its own keys. */
    it('"what" at 80 ms a key opens no chooser, and the band names w', () => {
      vi.useFakeTimers();
      try {
        const onChooseSessions = vi.fn();
        const onFlash = vi.fn();
        const rendered = renderEmpty({ onChooseSessions, onFlash });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        for (const key of ["w", "h", "a", "t"]) {
          fireEvent.keyDown(root, { key });
          act(() => vi.advanceTimersByTime(80));
        }
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
        expect(onChooseSessions).not.toHaveBeenCalled();
        expect(onFlash).toHaveBeenCalledWith(tableKeyTypingFlash("w", "all sessions"));
      } finally {
        vi.useRealTimers();
      }
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

      /** v1 hardening, codex-release-p1 #5 (agent-ui/web/src/components/EmptyTab.tsx:478): `f`
       *  intercepted every keydown unconditionally, ahead of the pending-sequence check -- so once
       *  `<leader>` (Space) armed a sequence, the very next `f` was stolen by HINT instead of
       *  continuing it, and the advertised `<leader>fn` (new tab) binding could never complete. */
      it("codex-release-p1 #5: Space f n completes <leader>fn instead of f stealing HINT mid-sequence", () => {
        const onPanelAction = vi.fn();
        const onHint = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction, onHint });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        fireEvent.keyDown(root, { key: "f" });
        fireEvent.keyDown(root, { key: "n" });
        expect(onPanelAction).toHaveBeenCalledWith(expect.objectContaining({ keys: ["<leader>", "f", "n"], action: "tab.new" }));
        expect(onHint).not.toHaveBeenCalled();
      });

      /** v1 hardening, codex-release-p1 #6 (R25 "an unbound key after the leader is swallowed",
       *  leader.ts:76-84): the dashboard's own leader engine mirrors `App.tsx`'s (its own doc
       *  comment says so) and shares the identical unguarded `seqRef.current !== null` branch --
       *  the same missing `isModifierKey` check that let a bare `Control`/`Shift` keydown cancel a
       *  pending sequence on the live tab. A bare Shift (as `gT`'s own, just above, and `Ctrl+c`'s
       *  in App.tsx) must not drop the armed `<leader>` here either. */
      it("a bare Shift keydown keeps a pending leader sequence alive, so Space Shift b d still completes <leader>bd", () => {
        const onPanelAction = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        fireEvent.keyDown(root, { key: "Shift", shiftKey: true });
        fireEvent.keyDown(root, { key: "b" });
        fireEvent.keyDown(root, { key: "d" });
        expect(onPanelAction).toHaveBeenCalledWith(expect.objectContaining({ keys: ["<leader>", "b", "d"], action: "tab.close" }));
      });

      /** Fix round 1 (reviewer finding, codex-release-p1 #3): the #6 fix above keeps a bare Control
       *  keydown from cancelling the pending sequence, but the REAL character keydown that follows
       *  it (`d`, carrying `ctrlKey: true`) used to reach `advanceSequence` as plain "d", completing
       *  `<leader>bd` for a Ctrl+d chord no table entry actually names. Space, b, Ctrl+d (the
       *  Control keydown included) must not run tab.close. */
      it("Space b Ctrl+d (the Control keydown included) does not run tab.close", () => {
        const onPanelAction = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        fireEvent.keyDown(root, { key: "b" });
        fireEvent.keyDown(root, { key: "Control", ctrlKey: true });
        fireEvent.keyDown(root, { key: "d", ctrlKey: true });
        expect(onPanelAction).not.toHaveBeenCalledWith(expect.objectContaining({ action: "tab.close" }));
      });

      /** The control for the test above: with no modifier at all, Space b d still runs tab.close --
       *  the fix narrows what a chord may complete, it does not touch the bare-key path. */
      it("Space b d (no modifier) still runs tab.close", () => {
        const onPanelAction = vi.fn();
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        fireEvent.keyDown(root, { key: "b" });
        fireEvent.keyDown(root, { key: "d" });
        expect(onPanelAction).toHaveBeenCalledWith(expect.objectContaining({ action: "tab.close" }));
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
      it("K01: a reserved prefix swallows a key with no pair: g then i opens no composer", () => {
        const rendered = renderEmpty({ panelTable: TABLE, onPanelAction: vi.fn() });
        const root = toBrowse(rendered, rendered.props);
        fireEvent.keyDown(root, { key: "g" });
        fireEvent.keyDown(root, { key: "i" });
        expect(rendered.container.querySelector("textarea")).toBeNull();
        fireEvent.keyDown(root, { key: "i" });
        expect(rendered.container.querySelector("textarea")).not.toBeNull();
      });
      /* Fix round 1 (review): a cancel route this screen never sees as a key -- Shift+Tab, which the
         window's document-capture router stops before this handler, an overlay, a tab switch --
         left its prefix and leader waiting, so `g`, Shift+Tab, `i` swallowed the `i`. The window
         now says so through `dropKeysRequest`. */
      it("K01: dropKeysRequest drops a waiting prefix and a leader sequence", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction, dropKeysRequest: 4 });
          const root = toBrowse(rendered, rendered.props);
          const at = (n: number) => rendered.rerender(<EmptyTab {...rendered.props} arriveRequest={1} dropKeysRequest={n} />);
          // Control: an unchanged count is no request, so the `g` still waits and swallows the `i`.
          fireEvent.keyDown(root, { key: "g" });
          at(4);
          fireEvent.keyDown(root, { key: "i" });
          expect(rendered.container.querySelector("textarea"), "the i ended the g").toBeNull();
          fireEvent.keyDown(root, { key: "g" });
          at(5);
          fireEvent.keyDown(root, { key: "i" });
          expect(rendered.container.querySelector("textarea"), "the request dropped the g").not.toBeNull();
          fireEvent.keyDown(rendered.container.querySelector("textarea")!, { key: "Escape" });
          act(() => vi.advanceTimersByTime(1000));
          fireEvent.keyDown(root, { key: " " });
          act(() => vi.advanceTimersByTime(1000));
          expect(rendered.container.querySelector(".which-key-box")).not.toBeNull();
          at(6);
          expect(rendered.container.querySelector(".which-key-box")).toBeNull();
          fireEvent.keyDown(root, { key: "m" });
          act(() => vi.advanceTimersByTime(1000));
          expect(onPanelAction, "<leader>m never ran").not.toHaveBeenCalled();
        } finally {
          vi.useRealTimers();
        }
      });
      it("K01: g, a pause, then f starts no HINT, and an input method's key drops the prefix", () => {
        vi.useFakeTimers();
        try {
          const onHint = vi.fn();
          const onPanelAction = vi.fn();
          const table = { ...TABLE, bindings: [...TABLE.bindings, { keys: ["g", "t"], action: "tab.next" as const, desc: "tab.next", source: "default" as const }] };
          const rendered = renderEmpty({ panelTable: table, onHint, onPanelAction });
          const root = toBrowse(rendered, rendered.props);
          fireEvent.keyDown(root, { key: "g" });
          act(() => vi.advanceTimersByTime(400));
          fireEvent.keyDown(root, { key: "f" }); // today: EmptyTab.tsx's `f` handler runs before the prefix check
          act(() => vi.advanceTimersByTime(1000));
          expect(onHint).not.toHaveBeenCalled();
          fireEvent.keyDown(root, { key: "g" });
          fireEvent.keyDown(root, { key: "d", isComposing: true }); // today: the IME return leaves `g` armed
          fireEvent.keyDown(root, { key: "t" });
          expect(onPanelAction).not.toHaveBeenCalled();
        } finally {
          vi.useRealTimers();
        }
      });

      /** v1 polish F16: vim's `gt`/`gT` reach the table from the dashboard too, `gT` through a bare
       *  Shift keydown that must not drop the pending `g`. */
      it("g t and g Shift T run the table's tab.next / tab.prev", () => {
        const onPanelAction = vi.fn();
        const table = {
          ...TABLE,
          bindings: [
            ...TABLE.bindings,
            { keys: ["g", "t"], action: "tab.next" as const, desc: "tab.next", source: "default" as const },
            { keys: ["g", "T"], action: "tab.prev" as const, desc: "tab.prev", source: "default" as const },
          ],
        };
        const rendered = renderEmpty({ panelTable: table, onPanelAction });
        const root = toBrowse(rendered, rendered.props);
        fireEvent.keyDown(root, { key: "g" });
        fireEvent.keyDown(root, { key: "t" });
        expect(onPanelAction).toHaveBeenLastCalledWith(expect.objectContaining({ keys: ["g", "t"], action: "tab.next" }));
        fireEvent.keyDown(root, { key: "g" });
        fireEvent.keyDown(root, { key: "Shift", shiftKey: true });
        fireEvent.keyDown(root, { key: "T", shiftKey: true });
        expect(onPanelAction).toHaveBeenLastCalledWith(expect.objectContaining({ keys: ["g", "T"], action: "tab.prev" }));
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

      /** Fix round 1 (reviewer finding, blocking; codex-release-p1 review, R2-1's own copy for this
       *  screen): `f` used to intercept every keydown unconditionally (codex-release-p1 #5's fix
       *  above only moved the pending-sequence check ahead of it), so a lone `f` asked for a HINT at
       *  once. It now defers the same `TYPING_GUARD_MS` the live conversation's own `f` does
       *  (`App.test.tsx`'s "f in BROWSE asks shell for a HINT..."). */
      it("f asks for a HINT TYPING_GUARD_MS later, not at once", () => {
        vi.useFakeTimers();
        try {
          const onHint = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onHint });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: "f" });
          expect(onHint).not.toHaveBeenCalled();
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
          expect(onHint).toHaveBeenCalledWith(false);
        } finally {
          vi.useRealTimers();
        }
      });

      /** Fix round 1's own reproduction (the blocking finding's failure scenario, R2-1 "scenario A"):
       *  "fix the dashboard layout⏎" typed on a fresh window's own first screen used to start a HINT
       *  on its own `f`. The `i` that follows within `TYPING_GUARD_MS` cancels the deferred `f`
       *  before it ever asks, and flashes the reason. */
      it('"fix this" at 80 ms a key asks for no HINT, and flashes', () => {
        vi.useFakeTimers();
        try {
          const onHint = vi.fn();
          const onFlash = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onHint, onFlash });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          for (const key of ["f", "i", "x"]) {
            fireEvent.keyDown(root, { key });
            act(() => vi.advanceTimersByTime(80));
          }
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
          expect(onHint).not.toHaveBeenCalled();
          // The cancelled `f` says so in its own words (the whole-branch review: this screen used to
          // drop what the guard's `onKey` returned, so it said nothing at all).
          expect(onFlash).toHaveBeenCalledWith(hintTypingFlash());
        } finally {
          vi.useRealTimers();
        }
      });

      /** `f` itself arriving soon after another key refuses at once and says so, mirroring
       *  `App.test.tsx`'s own "j then f at 80 ms a key". */
      it("j then f at 80 ms a key: f refuses at once, and flashes", () => {
        vi.useFakeTimers();
        try {
          const onHint = vi.fn();
          const onFlash = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onHint, onFlash });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: "j" });
          act(() => vi.advanceTimersByTime(80));
          fireEvent.keyDown(root, { key: "f" });
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
          expect(onHint).not.toHaveBeenCalled();
          expect(onFlash).toHaveBeenCalledWith(hintTypingFlash());
        } finally {
          vi.useRealTimers();
        }
      });

      /** Fix round 1: `f` must still work on a `failed` tab (there is no live composer to disturb
       *  once the tab has failed, this file's own `onKeyDown` doc comment) even though `mode` reads
       *  "input" there -- `modeRef`/`failedRef`'s own reasoning. */
      it("f still asks for a HINT on a failed tab, TYPING_GUARD_MS later", () => {
        vi.useFakeTimers();
        try {
          const onHint = vi.fn();
          const rendered = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "boom", panelTable: TABLE, onHint });
          const { container } = rendered;
          const root = container.querySelector(".empty-tab")!;
          fireEvent.keyDown(root, { key: "f" });
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
          expect(onHint).toHaveBeenCalledWith(false);
        } finally {
          vi.useRealTimers();
        }
      });

      /** Fix round 1 (reviewer finding, blocking; codex-release-p1 review, R2-2's own copy for this
       *  screen): `H` used to run at once, ahead of the dashboard's own fixed keys -- this test used
       *  to pin exactly that ("runs immediately"). It now defers the same `TYPING_GUARD_MS` the live
       *  conversation's own `H`/`L` do (`App.test.tsx`'s "H posts tab_verb prev..."), so a lone `H`
       *  still runs, just that much later. */
      it("a direct table binding (H) runs TYPING_GUARD_MS later, ahead of the dashboard's own fixed keys", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: "H" });
          expect(onPanelAction).not.toHaveBeenCalled();
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
          expect(onPanelAction).toHaveBeenCalledWith({ keys: ["H"], action: "tab.prev", desc: "tab.prev", source: "default" });
        } finally {
          vi.useRealTimers();
        }
      });

      /** v1 audit P2-A6, this screen's own copy: the deferred `H` above closes over the OLD table's
       *  `binding` at keydown time -- replacing `panelTable` mid-wait used to leave that stale
       *  callback armed (`useEffect(() => { clearSequence(); }, [panelTable])` cancels only a
       *  pending multi-key sequence, never this screen's own `typingGuard`'s deferred single-key
       *  wait), so it still ran once `TYPING_GUARD_MS` elapsed. Codex's saved probe reproduces the
       *  identical shape on `App.tsx`'s copy (`P2.audit.test.tsx`, "P2 drops a delayed table binding
       *  when the table is replaced"). */
      it("replacing the table mid-guard-window drops the deferred H binding entirely (P2-A6)", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: "H" });
          expect(onPanelAction).not.toHaveBeenCalled();
          const replaced = {
            ...TABLE,
            bindings: TABLE.bindings.map((b) => (b.keys.join("") === "H" ? { ...b, action: "tab.next" as const } : b)),
          };
          rendered.rerender(<EmptyTab {...props} arriveRequest={1} panelTable={replaced} />);
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
          expect(onPanelAction).not.toHaveBeenCalled();
        } finally {
          vi.useRealTimers();
        }
      });

      /** Fix round 1's own reproduction (the blocking finding's failure scenario): "Looks good, now
       *  add tests⏎" typed on a fresh window's own first screen used to switch tabs on its own `L` --
       *  this screen never got App.tsx's R2-2 fix at all. The `o` that follows within
       *  `TYPING_GUARD_MS` cancels the deferred `L` before it ever posts. */
      it('"Looks good" at 80 ms a key posts no tab_verb', () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          for (const key of ["L", "o", "o", "k", "s"]) {
            fireEvent.keyDown(root, { key });
            act(() => vi.advanceTimersByTime(80));
          }
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
          expect(onPanelAction).not.toHaveBeenCalledWith(expect.objectContaining({ action: "tab.next" }));
        } finally {
          vi.useRealTimers();
        }
      });

      /** `L` itself arriving soon after another key refuses at once and says so, mirroring
       *  `App.test.tsx`'s own "k then L at 80 ms a key". */
      it("k then H at 80 ms a key: H refuses at once, and flashes", () => {
        vi.useFakeTimers();
        try {
          const onPanelAction = vi.fn();
          const onFlash = vi.fn();
          const rendered = renderEmpty({ panelTable: TABLE, onPanelAction, onFlash });
          const { props } = rendered;
          const root = toBrowse(rendered, props);
          fireEvent.keyDown(root, { key: "k" });
          act(() => vi.advanceTimersByTime(80));
          fireEvent.keyDown(root, { key: "H" });
          act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
          expect(onPanelAction).not.toHaveBeenCalledWith(expect.objectContaining({ action: "tab.prev" }));
          expect(onFlash).toHaveBeenCalledWith(tableKeyTypingFlash("H", "tab.prev"));
        } finally {
          vi.useRealTimers();
        }
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

  /** v1 (spec `2026-09-27-v1-mode-design.md`, S2): the dashboard used to cycle the mode straight off
   *  a bare `m`, with none of the D11 typed-text guard the rest of the panel now has around a move
   *  into bypass. `App.tsx`'s document-capture Shift+Tab router is the only way in now; this screen
   *  never dispatches on the letter at all any more, at any timing. */
  describe("v1 mode: S2 via the prompt", () => {
    it("m in the menu posts nothing", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "m" });
      expect(props.onCycleMode).not.toHaveBeenCalled();
    });

    it("m then y posts nothing (an envelope arriving between them changes nothing here)", () => {
      const rendered = renderEmpty();
      const { props } = rendered;
      const root = toBrowse(rendered, props);
      fireEvent.keyDown(root, { key: "m" });
      // Stands in for an envelope landing between the two keys: this screen reads no envelope
      // directly (that is `App.tsx`'s job), so a re-render with the same props is the whole effect
      // one could have here, and it changes nothing about what the next key does.
      rendered.rerender(<EmptyTab {...props} arriveRequest={1} />);
      fireEvent.keyDown(root, { key: "y" });
      expect(props.onCycleMode).not.toHaveBeenCalled();
    });

    it("Space, m, y at 100ms with the envelope between m and y: still nothing", () => {
      vi.useFakeTimers();
      try {
        const rendered = renderEmpty();
        const { props } = rendered;
        const root = toBrowse(rendered, props);
        fireEvent.keyDown(root, { key: " " });
        act(() => vi.advanceTimersByTime(100));
        fireEvent.keyDown(root, { key: "m" });
        rendered.rerender(<EmptyTab {...props} arriveRequest={1} />);
        act(() => vi.advanceTimersByTime(100));
        fireEvent.keyDown(root, { key: "y" });
        expect(props.onCycleMode).not.toHaveBeenCalled();
      } finally {
        vi.useRealTimers();
      }
    });

    it("the Mode item still runs on Enter, and its label is the real way in (⇧Tab)", () => {
      const rendered = renderEmpty();
      const { props, container } = rendered;
      const root = toBrowse(rendered, props);
      // dashItems' own order with a resumable session present: new, resume, sessions, mode, keys.
      fireEvent.keyDown(root, { key: "j" });
      fireEvent.keyDown(root, { key: "j" });
      fireEvent.keyDown(root, { key: "j" });
      fireEvent.keyDown(root, { key: "Enter" });
      expect(props.onCycleMode).toHaveBeenCalledTimes(1);
      const keys = Array.from(container.querySelectorAll(".dash-key")).map((el) => el.textContent);
      expect(keys).toContain("⇧Tab");
      expect(keys).not.toContain("m");
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

/* K04 (2026-09-29): a tab failing while its composer holds the keys hands them to this screen's own
   root, in BROWSE, rather than leaving them in a disabled textarea for WebKit's focus fix-up to drop
   on <body>. */
describe("EmptyTab: K04, a failure while the composer has the keys", () => {
  it("focuses the root and reports BROWSE when the tab turns failed", () => {
    const onModeChange = vi.fn();
    const rendered = renderEmpty({ tab: { ...TAB, state: "starting" }, onModeChange, focusRequest: 1 });
    const textarea = rendered.container.querySelector("textarea")!;
    act(() => textarea.focus());
    expect(document.activeElement).toBe(textarea);
    rendered.rerender(
      <EmptyTab {...rendered.props} onModeChange={onModeChange} focusRequest={1} tab={{ ...TAB, state: "failed" }} failure="boom" />,
    );
    expect(document.activeElement).toBe(rendered.container.querySelector(".empty-tab"));
    expect(onModeChange).toHaveBeenLastCalledWith("browse");
    fireEvent.keyDown(document.activeElement!, { key: "r" });
    expect(rendered.props.onReset).toHaveBeenCalledTimes(1);
  });
  it("leaves the keys alone under an overlay", () => {
    const rendered = renderEmpty({ tab: { ...TAB, state: "starting" } });
    const textarea = rendered.container.querySelector("textarea")!;
    act(() => textarea.focus());
    rendered.rerender(<EmptyTab {...rendered.props} overlayOpen={true} tab={{ ...TAB, state: "failed" }} failure="boom" />);
    expect(document.activeElement).toBe(textarea);
  });
});
