// @vitest-environment jsdom
/**
 * Enter while a turn runs puts the message into that turn when the tab's agent can take it (capability
 * `steer`, Codex today), and holds it for the turn's end otherwise. Rust decides and reports: the page sends
 * the same `queue_message` either way, draws what Rust's queue envelope says (`↪` sent and not yet read,
 * `⧗` held, the reason once), and folds the agent's reading of the message as a user prompt where it
 * happened. The events, the snapshot and the queue envelope come from `fixtures/steered-messages.json`,
 * which Rust writes from its own serializer.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { Composer } from "./components/Composer";
import { QueueLines } from "./components/QueueLines";
import { MessageList } from "./components/MessageList";
import { heldCount, runningPlaceholder } from "./queueLines";
import { INPUT_KEYS } from "./keymap";
import { phaseOf } from "./turnPhase";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, Hello, QueueItem } from "./types";
import fixture from "./fixtures/steered-messages.json";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

describe("the agent reading a message put into its turn, folded", () => {
  it("the events Rust sends and its snapshot agree: one prompt where it was read, none for the dropped one", () => {
    const events = fixture.events.events as AgentDomainEvent[];
    expect(events.map((e) => e.type)).toContain("user_message_delivered");
    expect(events.map((e) => e.type)).toContain("user_message_dropped");
    let live = initialState();
    for (const event of events) live = applyEvent(live, event);
    const rebuilt = applySnapshot(initialState(), fixture.snapshot.state as unknown as AgentUiSnapshot, fixture.snapshot.throughRevision);
    expect(live.userPrompts).toEqual(rebuilt.userPrompts);
    expect(live.transcript).toEqual(rebuilt.transcript);
    expect(live.userPrompts.map((p) => p.text)).toEqual(["fix the parser", "and the tests"]);
    // After the call it followed, before the reply it led to.
    const delivered = live.userPrompts[1].seq;
    expect(live.toolCalls[0].seq).toBeLessThan(delivered);
    expect(live.transcript[1].seq).toBeGreaterThan(delivered);
    expect(live.transcript).toHaveLength(2);
    expect(rebuilt.capabilities.steer).toBe(true);
    // Both folds mark the message read inside the turn, and only it.
    expect(live.userPrompts.map((p) => p.delivered ?? false)).toEqual([false, true]);
  });

  it("draws the message read inside the turn as part of it: its own class, not a turn's opening", () => {
    const rebuilt = applySnapshot(initialState(), fixture.snapshot.state as unknown as AgentUiSnapshot, fixture.snapshot.throughRevision);
    const { container } = render(
      <MessageList state={rebuilt} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={() => {}} />,
    );
    const prompts = [...container.querySelectorAll(".row-prompt")];
    expect(prompts.map((p) => p.classList.contains("row-prompt-delivered"))).toEqual([false, true]);
    expect(prompts[1].textContent).toContain("and the tests");
  });

  it("a dropped message changes nothing but the counter", () => {
    const before = initialState();
    const after = applyEvent(before, { type: "user_message_dropped", client_message_id: "x", reason: "r" });
    expect(after).toEqual({ ...before, nextSeq: before.nextSeq + 1 });
  });
});

describe("the lines above the box", () => {
  const queue = fixture.queue as { items: QueueItem[]; error: string | null; note: string | null };

  it("draws a message in the turn as ↪ and a held one as ⧗, oldest first, with the reason once", () => {
    const { container } = render(<QueueLines items={queue.items} error={queue.error} note={queue.note} />);
    const lines = [...container.querySelectorAll(".queue-line")].map((l) => l.textContent);
    expect(lines).toEqual(["↪ look at the lexer too", "⧗ then commit"]);
    expect(container.querySelector(".queue-line")!.classList.contains("queue-line-steered")).toBe(true);
    expect(container.querySelectorAll(".queue-note")).toHaveLength(1);
    expect(container.querySelector(".queue-note")!.textContent).toBe(
      "held until the turn ends: the turn ended before Codex read it",
    );
  });

  it("a held command keeps its place among the messages going into the turn, with the words once", () => {
    const commands = fixture.commandQueue as { items: QueueItem[]; error: string | null; note: string | null };
    const { container } = render(<QueueLines items={commands.items} error={commands.error} note={commands.note} />);
    expect([...container.querySelectorAll(".queue-line")].map((l) => l.textContent)).toEqual([
      "↪ look at the lexer too",
      "⧗ /compact",
      "↪ then commit",
    ]);
    expect(container.querySelector(".queue-note")!.textContent).toBe("commands run when the turn ends");
    expect(heldCount(commands.items)).toBe(1);
  });

  it("says no reason when nothing is held", () => {
    const steered = queue.items.filter((i) => i.steered === true);
    const { container } = render(<QueueLines items={steered} error={null} note="held until the turn ends: x" />);
    expect(container.querySelector(".queue-note")).toBeNull();
    expect(heldCount(steered)).toBe(0);
    expect(heldCount(queue.items)).toBe(1);
  });
});

describe("Enter while a turn runs", () => {
  function box(enterSteers: boolean) {
    const onQueue = vi.fn();
    const onSendNow = vi.fn();
    const onSend = vi.fn();
    const { container } = render(
      <Composer
        disabled={false}
        sessionEnded={false}
        closing={false}
        restoredDraft={null}
        mode="input"
        onModeChange={vi.fn()}
        onSend={onSend}
        running
        enterSteers={enterSteers}
        onQueue={onQueue}
        onSendNow={onSendNow}
        slashCommands={[{ name: "review", description: "Review the changes" }]}
      />,
    );
    return { textarea: container.querySelector("textarea")!, onQueue, onSendNow, onSend, container };
  }

  it("says what it does in this tab, and hands the message to Rust either way", () => {
    for (const steers of [true, false]) {
      const { textarea, onQueue, onSend } = box(steers);
      expect(textarea.placeholder).toBe(runningPlaceholder(steers));
      fireEvent.change(textarea, { target: { value: "and the tests" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onQueue).toHaveBeenCalledWith("and the tests");
      expect(onSend).not.toHaveBeenCalled();
      cleanup();
    }
    expect(runningPlaceholder(true)).toBe("Enter steers the running turn…");
    expect(runningPlaceholder(false)).toBe("Enter holds it until the turn ends…");
  });

  it("Ctrl+Enter still sends now", () => {
    const { textarea, onSendNow, onQueue } = box(true);
    fireEvent.change(textarea, { target: { value: "stop, do this" } });
    fireEvent.keyDown(textarea, { key: "Enter", ctrlKey: true });
    expect(onSendNow).toHaveBeenCalledWith("stop, do this");
    expect(onQueue).not.toHaveBeenCalled();
  });

  it("the open / menu keeps Enter: it completes the name and steers nothing", () => {
    const { textarea, onQueue } = box(true);
    fireEvent.change(textarea, { target: { value: "/rev", selectionStart: 4 } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onQueue).not.toHaveBeenCalled();
    expect(textarea.value).toBe("/review ");
  });

  it("the key table says so", () => {
    const enter = INPUT_KEYS.find((k) => k.keys === "Enter")!;
    expect(enter.what).toContain("into that turn");
    expect(enter.what).toContain("held for its end when the agent cannot take it");
    expect(INPUT_KEYS.find((k) => k.keys === "↑ / ↓")!.what).toContain("the held messages back");
  });
});

describe("the panel with a message in the turn", () => {
  let posted: Array<Record<string, unknown>>;
  beforeEach(() => {
    posted = [];
    (window as unknown as { webkit: unknown }).webkit = {
      messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
    };
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  function dispatch(payload: unknown) {
    act(() => {
      window.__eitriDispatch!(JSON.stringify(payload));
    });
  }
  const HELLO: Hello = {
    backend: "sidecar",
    projectDir: "/project",
    permissionModes: ["auto", "bypass"],
    resumableSessions: [],
    account: null,
  };
  const TAB = {
    id: 1, number: 1, label: "1 codex", name: null, state: "live", mode: "auto",
    marker: null, pending: 0, resumable: false, failure: null, title: null, agent: "codex",
  } as const;

  function running(steer: boolean, items: QueueItem[], enterSteers?: boolean) {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
    const state: AgentUiState = {
      ...initialState(),
      status: { kind: "running" },
      activeTurnId: "t1",
      capabilities: { ...initialState().capabilities, interrupt: true, steer },
    };
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state });
    dispatch({ kind: "queue", tab: 1, items, error: null, note: null, ...(enterSteers === undefined ? {} : { enterSteers }) });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    return container;
  }

  it("draws both kinds of line and names the queued-message keys", () => {
    const container = running(true, [
      { text: "in the turn", queuedAt: 1, steered: true },
      { text: "held", queuedAt: 2, steered: false },
    ]);
    expect([...container.querySelectorAll(".queue-line")].map((l) => l.textContent)).toEqual(["↪ in the turn", "⧗ held"]);
    expect(container.querySelector("textarea")!.placeholder).toBe("↑ to edit queued messages, Ctrl+Enter to send them now");
  });

  it("↑ takes back the held messages, and with only a message in the turn walks history instead", () => {
    const container = running(true, [
      { text: "in the turn", queuedAt: 1, steered: true },
      { text: "held", queuedAt: 2, steered: false },
    ]);
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "ArrowUp" });
    expect(posted.filter((m) => m.type === "take_back_queue")).toHaveLength(1);
    cleanup();
    posted.length = 0;

    const only = running(true, [{ text: "in the turn", queuedAt: 1, steered: true }]);
    dispatch({ kind: "history", entries: ["an older prompt"] });
    const textarea = only.querySelector("textarea")!;
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(posted.filter((m) => m.type === "take_back_queue")).toHaveLength(0);
    expect(textarea.value).toBe("an older prompt");
  });

  it("a tab whose agent cannot take messages mid-turn says Enter holds", () => {
    const container = running(false, []);
    expect(container.querySelector("textarea")!.placeholder).toBe("Enter holds it until the turn ends…");
  });

  it("sends the turn it showed with the message", () => {
    const container = running(true, []);
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "and the tests" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    const sent = posted.filter((m) => m.type === "queue_message");
    expect(sent[sent.length - 1]).toMatchObject({ text: "and the tests", turn_id: "t1" });
  });

  it("follows the queue envelope when the agent stops taking messages mid-turn", () => {
    const container = running(true, [], false);
    expect(container.querySelector("textarea")!.placeholder).toBe("Enter holds it until the turn ends…");
  });
});

describe("the activity line while a message is read mid-turn", () => {
  it("a prompt the agent read during a running call does not hide that call", () => {
    const state: AgentUiState = {
      ...initialState(),
      status: { kind: "running" },
      activeTurnId: "t2",
      userPrompts: [
        { seq: 1, text: "go" },
        { seq: 5, text: "and the tests" },
      ],
      toolCalls: [{ seq: 4, toolUseId: "c", name: "Bash", input: {}, result: null, turnId: "t2" }],
    };
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    // A call left open by an earlier turn is still displaced by the next turn's prompt.
    const next: AgentUiState = { ...state, activeTurnId: "t3", toolCalls: [{ ...state.toolCalls[0], turnId: "t1" }] };
    expect(phaseOf(next)).toEqual({ kind: "sent" });
  });
});
