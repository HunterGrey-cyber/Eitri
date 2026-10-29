// @vitest-environment jsdom
import { createRef } from "react";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initialState } from "../reducer";
import { completionSource, slashAction } from "../slashCommands";
import { TASKS_KEY_LINE } from "../keymap";
import type { AgentUiState, BackgroundTaskRecord, TaskKind } from "../types";
import { phaseLine, TasksOverlay, taskOutputText, type TasksOverlayHandle } from "./TasksOverlay";

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(100_000);
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

function task(id: string, kind: TaskKind = "shell", extra: Partial<BackgroundTaskRecord> = {}): BackgroundTaskRecord {
  return { task_id: id, run_id: "1", kind, task_type: kind, description: id, subagent_type: null,
    parent_task_id: null, tool_use_id: null, ambient: false, workflow_name: null, status: "running", live: true,
    started_at: 60_000, ended_at: null, finished_seq: null, summary: null, last_tool_name: null, usage: null,
    error: null, is_backgrounded: true, reason: null, output_file: null, workflow_progress: null, ...extra };
}
function state(rows: BackgroundTaskRecord[]): AgentUiState {
  return { ...initialState(), capabilities: { ...initialState().capabilities, backgroundTasks: true, taskControl: true,
    taskOutput: true, workflowProgress: true }, backgroundTasks: { rows, query_start: false } };
}
function setup(current = state([task("build"), task("watch", "monitor")]), extra = {}) {
  const ref = createRef<TasksOverlayHandle>();
  const onRequest = vi.fn(), onOpen = vi.fn(), onClose = vi.fn();
  const props = { state: current, onRequest, onOpen, onClose, ...extra };
  const rendered = render(<TasksOverlay ref={ref} {...props} />);
  const dialog = () => rendered.getByRole("dialog");
  const key = (key: string, fields = {}) => fireEvent.keyDown(dialog(), { key, ...fields });
  const reply = (fields = {}) => act(() => ref.current!.reply({ requestId: onRequest.mock.lastCall![2], ...fields }));
  return { ...rendered, ref, dialog, key, reply, onRequest, onOpen, onClose,
    update: (next: AgentUiState) => rendered.rerender(<TasksOverlay ref={ref} {...props} state={next} />) };
}

describe("background tasks", () => {
  it("groups live work before recent completions and folds system work", () => {
    const view = setup(state([task("workflow", "workflow"), task("monitor", "monitor"), task("agent", "local_agent"),
      task("shell"), task("done", "shell", { live: false, status: "completed", ended_at: 99_000 }),
      task("housekeeping", "shell", { ambient: true })]));
    expect(Array.from(view.container.querySelectorAll("h3")).map((e) => e.textContent)).toEqual([
      "Shells (1)", "Monitors (1)", "Local agents (1)", "Dynamic workflows (1)", "Completed (1)",
    ]);
    expect(view.queryByText("housekeeping")).toBeNull();
    fireEvent.click(view.getByText("▸ System tasks (1)"));
    expect(view.getByText("housekeeping")).toBeTruthy();
    expect(view.getByText(TASKS_KEY_LINE)).toBeTruthy();
  });

  it("keeps a completed row for thirty seconds on a running clock", () => {
    const view = setup(state([task("done", "shell", { status: "completed", live: false, ended_at: 100_000 })]));
    act(() => vi.advanceTimersByTime(29_000));
    expect(view.getByText("done")).toBeTruthy();
    act(() => vi.advanceTimersByTime(1_000));
    expect(view.queryByText("done")).toBeNull();
  });

  it("returns to the list if the task being viewed expires", () => {
    const view = setup(state([task("done", "shell", { status: "completed", live: false, ended_at: 100_000 }), task("live")]));
    view.key("j"); view.key("Enter");
    expect(view.getByRole("heading", { name: "✓ done" })).toBeTruthy();
    act(() => vi.advanceTimersByTime(30_000));
    expect(view.getByRole("listbox")).toBeTruthy();
    expect(view.queryByText("done")).toBeNull();
  });

  it.each([["j", "k"], ["ArrowDown", "ArrowUp"]])("%s and %s move the selection", (down, up) => {
    const view = setup();
    view.key(down);
    expect(view.getByRole("option", { selected: true }).textContent).toContain("watch");
    view.key(up);
    expect(view.getByRole("option", { selected: true }).textContent).toContain("build");
    view.key(up);
    expect(view.getByRole("option", { selected: true }).textContent).toContain("build");
  });

  it.each(["q", "Escape"])("Enter views; %s goes back then closes", (back) => {
    const view = setup();
    view.key("Enter");
    expect(view.queryByRole("listbox")).toBeNull();
    expect(view.onRequest).toHaveBeenCalledWith("task_output", "build", expect.any(String));
    view.key(back);
    expect(view.getByRole("listbox")).toBeTruthy();
    expect(view.onClose).not.toHaveBeenCalled();
    view.key(back);
    expect(view.onClose).toHaveBeenCalledOnce();
  });

  it("x stops immediately in the list and the detail, but held or modified x does not", () => {
    const view = setup();
    view.key("x", { repeat: true }); view.key("x", { ctrlKey: true }); view.key("x", { isComposing: true });
    expect(view.onRequest).not.toHaveBeenCalled();
    view.key("x");
    expect(view.onRequest).toHaveBeenLastCalledWith("stop_task", "build", expect.any(String));
    view.reply();
    view.key("Enter");
    view.key("x");
    expect(view.onRequest).toHaveBeenLastCalledWith("stop_task", "build", expect.any(String));
  });

  it("o reads fresh output and opens sanitized plain text in the editor", () => {
    const view = setup();
    view.key("o");
    view.reply({ taskId: "build", output: "\x1b[31m<b>red</b>\x1b[0m\u202e", totalBytes: 30, truncated: true });
    expect(view.onOpen).toHaveBeenCalledWith("Task output", "<b>red</b>⟨U+202E⟩");
    view.key("Enter");
    view.reply({ taskId: "build", output: "tail\u200b <img src=x>\x1b]8;;http://evil\x07label\x1b]8;;\x07", totalBytes: 9000, truncated: true });
    expect(view.getByText("tail⟨U+200B⟩ <img src=x>label")).toBeTruthy();
    expect(view.getByText("tail of 9000 bytes")).toBeTruthy();
    expect(view.container.querySelector("img, a, b")).toBeNull();
  });

  it("strips CSI, OSC and terminal string escapes before revealing hidden characters", () => {
    expect(taskOutputText("\x9b31mred\x9b0m\x1bPsecret\x1b\\\x9dtitle\x9c\u202e" )).toBe("red⟨U+202E⟩");
  });

  it("shows the full path as dim text and explains a background time-limit stop", () => {
    const view = setup(state([task("build", "shell", { status: "stopped", live: false, ended_at: 99_000,
      output_file: "/logs/\u200boutput", summary: 'Background command "build" was stopped after reaching its background time limit' })]));
    view.key("Enter");
    expect(view.getByText("/logs/⟨U+200B⟩output").className).toBe("tasks-dim");
    expect(view.getByText(/Claude Code stops a background command/)).toBeTruthy();
    view.key("x");
    expect(view.getByText("this task has ended")).toBeTruthy();
    expect(view.onRequest.mock.calls.every(([kind]) => kind !== "stop_task")).toBe(true);
  });

  it("opens workflows at their group, with phases, agent facts and plain launch text", () => {
    const current = state([task("build"), task("flow", "workflow", { tool_use_id: "launch", workflow_progress: {
      phases: [{ index: 0, title: "Plan\u202e", kind: "parallel" }],
      agents: [{ index: 0, label: "Worker", phase_index: 0, state: "failed", model: "opus", agent_type: null,
        tokens: 42, tool_calls: 3, duration_ms: 1000, error: "<b>error</b>" }],
    } })]);
    current.toolCalls = [{ seq: 0, toolUseId: "launch", name: "Workflow", input: {},
      result: { content: "Launch text: <script>directory</script>", isError: false } }];
    const view = setup(current, { workflows: true });
    expect(view.getByRole("option", { selected: true }).textContent).toContain("flow");
    view.key("Enter");
    expect(view.getByText("Plan⟨U+202E⟩ · parallel · 1 agent")).toBeTruthy();
    expect(view.getByText("Worker · failed · opus · 42 tokens · 3 tool calls · <b>error</b>")).toBeTruthy();
    expect(view.getByText("Launch text: <script>directory</script>")).toBeTruthy();
    expect(view.container.querySelector("script, b")).toBeNull();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("without workflow progress shows description, status, elapsed and usage", () => {
    const current = state([task("flow", "workflow", { usage: { total_tokens: 10, tool_uses: 2, duration_ms: 1000 } })]);
    current.capabilities.workflowProgress = false;
    const view = setup(current);
    view.key("Enter");
    expect(view.getByRole("heading", { name: "● flow" })).toBeTruthy();
    expect(view.getByText("running · 10 tokens · 2 tool calls · 0m 40s")).toBeTruthy();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("shows agent summary, last tool and usage without requesting an output file", () => {
    const view = setup(state([task("worker", "local_agent", { summary: "done\u202e", last_tool_name: "Read",
      usage: { total_tokens: 12, tool_uses: 2, duration_ms: 0 } })]));
    view.key("Enter");
    expect(view.getByText("done⟨U+202E⟩")).toBeTruthy();
    expect(view.getByText("Last tool: Read")).toBeTruthy();
    view.key("o");
    expect(view.getByText("this task has no output file")).toBeTruthy();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("shows an agent's child calls as a nested read-only list with escaped plain inputs and results", () => {
    const current = state([task("worker", "local_agent", { tool_use_id: "nested-launch" })]);
    const stream = (toolCalls: AgentUiState["toolCalls"]) => ({ transcript: [], thinking: [], toolCalls,
      assistantMessageOpen: false, thinkingOpen: false });
    current.toolCalls = [{ seq: 0, toolUseId: "outer", name: "Agent", input: {}, result: null,
      children: stream([{ seq: 1, toolUseId: "nested-launch", name: "Agent", input: {}, result: null,
        children: stream([
          { seq: 2, toolUseId: "read", name: "Read\u202e", input: { file_path: "<img src=x>\u200b" },
            result: { content: "<b>file</b>\u202e", isError: false } },
          { seq: 3, toolUseId: "agent", name: "Agent", input: { description: "Inspect" }, result: null,
            children: stream([{ seq: 4, toolUseId: "bash", name: "Bash", input: { command: "make" },
              result: { content: "<script>failed</script>\u200b", isError: true } }]) },
        ]) }]) },
      { seq: 5, toolUseId: "unrelated", name: "Unrelated", input: {}, result: null }];
    const view = setup(current);
    view.key("Enter");
    const lists = view.getAllByRole("list", { name: "Agent steps" });
    expect(lists).toHaveLength(2);
    expect(lists[0].querySelectorAll(":scope > li")).toHaveLength(2);
    expect(lists[0].querySelector("li ol")).toBe(lists[1]);
    expect(view.getByText("Read⟨U+202E⟩ · completed")).toBeTruthy();
    expect(view.getByText('<b>file</b>⟨U+202E⟩')).toBeTruthy();
    expect(view.getByText("Bash · failed")).toBeTruthy();
    expect(view.getByText('<script>failed</script>⟨U+200B⟩')).toBeTruthy();
    expect(lists[0].textContent).toContain('<img src=x>⟨U+200B⟩');
    expect(lists[0].querySelector("button, input, a, script, b, img")).toBeNull();
    expect(view.queryByText("Unrelated")).toBeNull();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("leaves an agent with no recorded child calls without a steps list", () => {
    const view = setup(state([task("worker", "local_agent")]));
    view.key("Enter");
    expect(view.queryByRole("list", { name: "Agent steps" })).toBeNull();
  });

  it("without background-task reporting states the limitation", () => {
    const current = state([task("hidden")]); current.capabilities.backgroundTasks = false;
    const view = setup(current);
    expect(view.getByText("this Claude Code sidecar does not report background work")).toBeTruthy();
    expect(view.queryByText("hidden")).toBeNull();
    view.key("x"); view.key("o");
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("without task control x explains why it cannot stop", () => {
    const current = state([task("build")]); current.capabilities.taskControl = false;
    const view = setup(current);
    view.key("x");
    expect(view.getByText("this sidecar cannot stop one task")).toBeTruthy();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("without task output Enter and o explain why it cannot read", () => {
    const current = state([task("build")]); current.capabilities.taskOutput = false;
    const view = setup(current);
    view.key("Enter"); view.key("o");
    expect(view.getByText("output is not available from this sidecar")).toBeTruthy();
    expect(view.onRequest).not.toHaveBeenCalled();
  });

  it("counts wakes below and reports bounded command failures as text", () => {
    const view = setup();
    act(() => view.ref.current!.wake(1));
    expect(view.getByText("1 update below")).toBeTruthy();
    view.key("Enter");
    view.reply({ error: "<b>unavailable</b>\u200b" });
    expect(view.getByText("<b>unavailable</b>⟨U+200B⟩")).toBeTruthy();
    expect(view.container.querySelector("b")).toBeNull();
  });

  it("ignores a late tail after selection changes or the task id starts a new run", () => {
    const view = setup();
    view.key("Enter");
    const firstId = view.onRequest.mock.lastCall![2];
    view.key("q"); view.key("j"); view.key("Enter");
    act(() => view.ref.current!.reply({ requestId: firstId, taskId: "build", output: "old" }));
    expect(view.queryByText("old")).toBeNull();
    view.update(state([task("watch", "monitor", { run_id: "2" })]));
    view.reply({ taskId: "watch", output: "previous run" });
    expect(view.queryByText("previous run")).toBeNull();
  });

  it("starts directly on a live card's task and reads its output", () => {
    const view = setup(undefined, { initialTaskId: "watch" });
    expect(view.getByRole("heading", { name: "● watch" })).toBeTruthy();
    expect(view.onRequest).toHaveBeenCalledWith("task_output", "watch", expect.any(String));
  });

  it("keyboard selection reaches and unfolds System tasks", () => {
    const view = setup(state([task("system", "other", { ambient: true })]));
    view.key("j"); view.key("Enter");
    expect(view.getByText("system")).toBeTruthy();
  });

  it("all seven bare words resolve locally, including reported task commands and the alias", () => {
    for (const name of ["help", "status", "resume", "memory", "rewind", "tasks", "workflows", "bashes"]) {
      expect(slashAction(`/${name}`)).not.toBeNull();
      expect(slashAction(`/${name} arg`)).toBeNull();
    }
    expect(slashAction("/workflows")).toEqual({ kind: "tasks", workflows: true });
    expect(completionSource("claude", [{ name: "tasks", description: "CLI" }, { name: "workflows", description: "CLI" }]))
      .toEqual([{ name: "tasks", description: "Show background tasks" }, { name: "workflows", description: "Show dynamic workflows" }]);
  });

  it("/workflows starts on a live workflow, not on a completed one that has expired", () => {
    const view = setup(state([
      task("old", "workflow", { live: false, status: "completed", ended_at: 10_000 }),
      task("shell"), task("flow", "workflow"),
    ]), { workflows: true });
    expect(view.container.querySelector("[aria-selected=true]")!.textContent).toContain("flow");
    view.key("x");
    expect(view.onRequest).toHaveBeenCalledWith("stop_task", "flow", expect.any(String));
  });

  it("a row ended with no clock time stays thirty seconds from when it was first seen and shows no elapsed", () => {
    const view = setup(state([task("done", "shell", { status: "completed", live: false, started_at: 3, ended_at: 7, finished_seq: 7 })]));
    act(() => vi.advanceTimersByTime(29_000));
    expect(view.getByText("done")).toBeTruthy();
    expect(view.container.querySelector(".tasks-elapsed")!.textContent).toBe("");
    act(() => vi.advanceTimersByTime(1_000));
    expect(view.queryByText("done")).toBeNull();
  });

  it("scrolls a detail view taller than the overlay with the movement and page keys", () => {
    const view = setup(state([task("flow", "workflow")]));
    view.key("Enter");
    const overlay = view.dialog();
    Object.defineProperty(overlay, "scrollHeight", { value: 5000, configurable: true });
    Object.defineProperty(overlay, "clientHeight", { value: 400, configurable: true });
    view.key("j"); expect(overlay.scrollTop).toBe(40);
    view.key("ArrowDown"); expect(overlay.scrollTop).toBe(80);
    view.key("k"); expect(overlay.scrollTop).toBe(40);
    view.key("PageDown"); expect(overlay.scrollTop).toBe(40 + 360);
    view.key("PageUp"); expect(overlay.scrollTop).toBe(40);
    view.key("End"); expect(overlay.scrollTop).toBe(5000);
    view.key("Home"); expect(overlay.scrollTop).toBe(0);
    expect(view.getByRole("heading", { name: "● flow" })).toBeTruthy();
  });

  it("an AltGraph-modified x is not a stop", () => {
    const view = setup(state([task("build")]));
    view.key("x", { modifierAltGraph: true });
    expect(view.onRequest).not.toHaveBeenCalled();
    view.key("x");
    expect(view.onRequest).toHaveBeenCalledWith("stop_task", "build", expect.any(String));
  });
});

it("words a workflow phase without an empty field and counts its agents in the singular for one", () => {
  expect(phaseLine({ title: "one", kind: "" }, 1)).toBe("one · 1 agent");
  expect(phaseLine({ title: "two", kind: "parallel" }, 2)).toBe("two · parallel · 2 agents");
  expect(phaseLine({ title: "three", kind: "" }, 0)).toBe("three · 0 agents");
});
