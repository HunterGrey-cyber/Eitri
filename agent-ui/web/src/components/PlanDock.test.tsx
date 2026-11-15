// @vitest-environment jsdom
import { createRef } from "react";
import * as markdown from "../markdown";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PlanDock, type PlanDockHandle } from "./PlanDock";
import type { PermissionModeChoice, PlanRecord } from "../types";

const SHA = "a".repeat(64);
const record = (plan = "# Plan\n\n1. Read `src/a.rs`\n2. Change it", extra: Partial<PlanRecord> = {}): PlanRecord => ({
  seq: 3, interactionId: "p1", toolUseId: "toolu_exit", turnId: "t1", plan, planSha256: SHA,
  allowedPrompts: [], state: { kind: "waiting" }, unsupportedReason: null, ...extra,
});
let messages: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers();
  vi.spyOn(performance, "now").mockImplementation(() => Date.now());
  messages = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });
function pause(ms: number) { act(() => { vi.advanceTimersByTime(ms); }); }
let refused: string[];
function setup(options: { plan?: PlanRecord; origin?: PermissionModeChoice | null; draft?: string; blocked?: () => boolean } = {}) {
  const ref = createRef<PlanDockHandle>();
  refused = [];
  const dockOf = (active: boolean) => <PlanDock ref={ref} tab={1} record={options.plan ?? record()} origin={options.origin ?? "manual"}
    draft={options.draft} active={active} blocked={options.blocked ?? (() => false)} onActivate={() => {}} stillWaiting={() => true}
    onRefused={(why) => refused.push(why)} />;
  const result = render(dockOf(true));
  const dock = result.getByRole("group");
  const press = (key: string, rest = {}, target: Element = document.activeElement ?? dock) => {
    fireEvent.keyDown(target, { key, ...rest });
  };
  return { ...result, ref, dock, press, activate: (active: boolean) => result.rerender(dockOf(active)) };
}
const resolves = () => messages.filter((m) => m.type === "plan_resolve");
const focused = (container: HTMLElement) => container.querySelector<HTMLElement>('.plan-action[data-focused="true"]')?.textContent;

describe("plan dock", () => {
  it("Keep planning has focus first and Enter during the guard does nothing", () => {
    const { container, press } = setup();
    expect(focused(container)).toContain("Keep planning");
    press("Tab");
    expect(focused(container)).toContain("Approve (Manual)");
    // Not yet painted long enough, and Tab was pressed just now.
    press("Enter");
    expect(resolves()).toEqual([]);
    pause(100); press("Enter");
    expect(resolves()).toEqual([]);
    // A lone Enter after the guard approves.
    pause(300); press("Enter");
    expect(resolves()).toEqual([{ type: "plan_resolve", request_id: expect.any(String), tab: 1, interaction_id: "p1", choice: "approve_manual", plan_sha256: SHA }]);
  });
  it("Approve and keep Bypass appears only for a Bypass origin and is never focused first", () => {
    for (const origin of ["manual", "auto", null] as const) {
      const { container, unmount } = setup({ origin });
      expect(container.querySelector('[data-choice="approve_bypass"]')).toBeNull();
      expect([...container.querySelectorAll(".plan-action")].map((b) => b.getAttribute("data-choice")))
        .toEqual(["keep_planning", "approve_manual", "approve_auto"]);
      unmount();
    }
    const { container, press } = setup({ origin: "bypass" });
    expect([...container.querySelectorAll(".plan-action")].map((b) => b.getAttribute("data-choice")))
      .toEqual(["keep_planning", "approve_manual", "approve_auto", "approve_bypass"]);
    expect(focused(container)).toContain("Keep planning");
    // Moving backward at the first action keeps the focus there.
    press("Tab", { shiftKey: true });
    expect(focused(container)).toContain("Keep planning");
    for (let i = 0; i < 3; i += 1) press("Tab");
    expect(focused(container)).toContain("Approve and keep Bypass");
    pause(300); press("Enter");
    expect(resolves()[0]).toMatchObject({ choice: "approve_bypass" });
  });
  it("no digit or letter approves", () => {
    const { press } = setup({ origin: "bypass" });
    pause(300);
    for (const key of ["1", "2", "3", "4", "y", "Y", "a", "A", "n", " ", "d", "D"]) { press(key); pause(300); }
    press("y", { ctrlKey: true });
    expect(resolves()).toEqual([]);
  });
  it("keep planning sends the feedback and a refusal restores it", () => {
    const { container, press, ref, getByLabelText } = setup();
    pause(300); press("Enter");
    expect(resolves()).toEqual([]);
    pause(10);
    const field = getByLabelText("What to change in the plan (optional)") as HTMLTextAreaElement;
    expect(document.activeElement).toBe(field);
    fireEvent.change(field, { target: { value: "Shorter, please\nand no new files" } });
    pause(400);
    expect(messages.filter((m) => m.type === "plan_feedback_draft")).toEqual([
      { type: "plan_feedback_draft", request_id: expect.stringMatching(/mirror/), tab: 1, interaction_id: "p1", text: "Shorter, please\nand no new files" },
    ]);
    // Esc goes back to the actions with the text kept.
    press("Escape", {}, field);
    expect(document.activeElement?.classList.contains("plan-dock")).toBe(true);
    expect(field.value).toBe("Shorter, please\nand no new files");
    press("Enter", { ctrlKey: true }, field);
    expect(resolves()).toEqual([{ type: "plan_resolve", request_id: expect.any(String), tab: 1, interaction_id: "p1",
      choice: "keep_planning", plan_sha256: SHA, feedback: "Shorter, please\nand no new files" }]);
    expect(field.disabled).toBe(true);
    act(() => { ref.current!.reply({ kind: "command_result", requestId: String(resolves()[0].request_id), ok: false, error: "Try again" }); });
    pause(10);
    expect(container.querySelector(".plan-footer")?.textContent).toBe("Try again");
    expect(field.disabled).toBe(false);
    expect(field.value).toBe("Shorter, please\nand no new files");
    expect(document.activeElement).toBe(field);
  });
  it("the feedback restored from the tab row opens with it", () => {
    const { getByLabelText } = setup({ draft: "kept from before" });
    expect((getByLabelText("What to change in the plan (optional)") as HTMLTextAreaElement).value).toBe("kept from before");
    pause(400);
    expect(messages.filter((m) => m.type === "plan_feedback_draft")).toEqual([]);
  });
  it("a malicious plan renders inert: no script, no remote image, no form, hidden characters shown", () => {
    const evil = [
      "# Plan‮​", "<script>window.pwned = 1</script>", "<iframe src=\"https://evil.example\"></iframe>",
      "![x](https://evil.example/pixel.png)", "<img src=\"http://evil.example/a.png\" onerror=\"window.pwned = 2\">",
      "<form action=\"https://evil.example\"><button>Approve</button></form>", "<a href=\"javascript:alert(1)\">run</a>",
      "Step with zero​width",
    ].join("\n\n");
    const { container } = setup({ plan: record(evil) });
    const body = container.querySelector(".plan-body")!;
    expect(body.querySelector("script, iframe, form, img[src^=http], [onerror]")).toBeNull();
    expect(body.querySelector('a[href^="javascript"]')).toBeNull();
    // The remote image is a link to it, not a fetch.
    expect(body.querySelector('a[href="https://evil.example/pixel.png"]')).toBeTruthy();
    expect(body.textContent).toContain("⟨U+200B⟩");
    expect(body.textContent).toContain("⟨U+202E⟩");
    // The only buttons are the dock's own.
    expect([...container.querySelectorAll("button")].every((b) => b.closest(".plan-actions, .plan-feedback"))).toBe(true);
  });
  it("allowed prompts are plain text", () => {
    const plan = record(undefined, { allowedPrompts: [
      { tool: "Bash", prompt: "run the **tests** <b>now</b>" }, { tool: "Bash", prompt: "remote‮​" },
    ] });
    const { container, getByText } = setup({ plan });
    const list = container.querySelector(".plan-prompts")!;
    expect(getByText("Claude says it will need")).toBeTruthy();
    expect(list.querySelector("b, strong, a, button")).toBeNull();
    expect(list.textContent).toContain("run the **tests** <b>now</b>");
    expect(list.textContent).toContain("⟨U+202E⟩");
  });
  it("a long plan scrolls by keys and is not cut", () => {
    const steps = Array.from({ length: 400 }, (_, i) => `${i + 1}. step number ${i + 1}`).join("\n");
    const { container, press } = setup({ plan: record(`# Plan\n\n${steps}\n\nTHE LAST LINE`) });
    const body = container.querySelector<HTMLElement>(".plan-body")!;
    expect(body.textContent).toContain("step number 400");
    expect(body.textContent).toContain("THE LAST LINE");
    Object.defineProperty(body, "scrollHeight", { configurable: true, value: 10_000 });
    Object.defineProperty(body, "clientHeight", { configurable: true, value: 300 });
    press("j"); expect(body.scrollTop).toBe(40);
    press("d", { ctrlKey: true }); expect(body.scrollTop).toBe(190);
    press("k"); expect(body.scrollTop).toBe(150);
    press("u", { ctrlKey: true }); expect(body.scrollTop).toBe(0);
    press("G", { shiftKey: true }); expect(body.scrollTop).toBe(10_000);
    press("g"); expect(body.scrollTop).toBe(10_000);
    press("g"); expect(body.scrollTop).toBe(0);
    expect(resolves()).toEqual([]);
  });
  it("every action is reachable by keys alone, and none takes the keys itself", () => {
    const { container, press } = setup({ origin: "bypass" });
    const seen: string[] = [];
    for (let i = 0; i < 4; i++) { seen.push(focused(container)!); press("Tab"); }
    expect(seen.map((label) => label.slice(1))).toEqual(["Keep planning", "Approve (Manual)", "Approve (Auto)", "Approve and keep Bypass"]);
    expect(focused(container)).toContain("Approve and keep Bypass");
    for (const button of container.querySelectorAll<HTMLButtonElement>(".plan-dock button")) {
      expect(button.tabIndex).toBe(-1);
      // A press does not move the keys onto the button, where a modified Enter could press it past the guard.
      expect(fireEvent.mouseDown(button)).toBe(false);
    }
    expect(container.querySelector(".plan-dock")!.getAttribute("aria-activedescendant")).toBe(container.querySelector('.plan-action[data-focused="true"]')!.id);
  });
  it("a modified Enter or Space decides nothing and presses no button", () => {
    const { dock } = setup();
    for (const mod of [{ altKey: true }, { metaKey: true }, { ctrlKey: true }]) {
      expect(fireEvent.keyDown(dock, { key: "Enter", ...mod })).toBe(false);
      expect(fireEvent.keyDown(dock, { key: " ", ...mod })).toBe(false);
    }
    pause(300);
    expect(resolves()).toEqual([]);
  });
  it("an Enter dropped by the guard says why; the guard starts over when the dock takes the keys again", () => {
    const { container, press, activate } = setup({ origin: "bypass" });
    pause(300); press("Tab"); press("Enter");
    expect(resolves()).toEqual([]);
    expect(refused).toHaveLength(1);
    pause(300); press("Tab"); press("Tab"); pause(300);
    expect(focused(container)).toContain("Approve and keep Bypass");
    // Back to BROWSE and in again: on Keep planning, and an Enter right away is no decision.
    activate(false);
    expect(focused(container)).toContain("Keep planning");
    pause(1000);
    activate(true);
    press("Enter");
    expect(resolves()).toEqual([]);
    expect(container.querySelector(".plan-feedback")).toBeNull();
  });
  it("the plan is drawn once, not on every key", () => {
    const draw = vi.spyOn(markdown, "renderMarkdownRevealed");
    const { press } = setup({ plan: record("# Once\n\nzero\u200bwidth") });
    for (const key of ["j", "Tab", "k", "Tab"]) press(key);
    expect(draw).toHaveBeenCalledTimes(1);
  });
  it("prose shows what disguises a sentence, not how emoji and spaces are written", () => {
    const { container } = setup({ plan: record("\u26A0\uFE0F Risky \u{1F468}\u200D\u{1F4BB} step\u00A0one\u202Ftwo\u3000three\n\nhidden\u200Bhere \u202Edrawkcab\n\n`a\u00A0b`") });
    const body = container.querySelector(".plan-body")!;
    const escapes = [...body.querySelectorAll(".code-escape")].map((e) => e.textContent);
    expect(escapes).toEqual(["⟨U+200B⟩", "⟨U+202E⟩", "⟨U+00A0⟩"]);
  });
  it("a plan shows the hidden characters the page holds: decoded references and tags that draw no flag", () => {
    const tags = [..."delete checks"].map((c) => String.fromCodePoint(0xe0000 + c.codePointAt(0)!)).join("");
    const { container } = setup({ plan: record(`Keep &#x202e;secret&#x202c; visible\n\n\u2705${tags} Run the suite`) });
    const body = container.querySelector(".plan-body")!;
    const escapes = [...body.querySelectorAll(".code-escape")].map((e) => e.textContent);
    expect(escapes.slice(0, 2)).toEqual(["⟨U+202E⟩", "⟨U+202C⟩"]);
    expect(escapes).toHaveLength(2 + [...tags].length);
    for (const e of body.querySelectorAll(".code-escape")) e.remove();
    expect(body.textContent).not.toMatch(/[\u202A-\u202E\u{E0000}-\u{E007F}]/u);
  });
  it("a plan with too many hidden characters is shown as escaped plain text", () => {
    const { container } = setup({ plan: record(`# Plan\n\n${"a\u200b".repeat(markdown.MAX_PROSE_ESCAPES + 1)}`) });
    expect(container.textContent).toContain("Too many hidden characters");
    const body = container.querySelector("pre.plan-body")!;
    expect(body.querySelector("*")).toBeNull();
    expect(body.textContent!.startsWith("# Plan\n\na⟨U+200B⟩")).toBe(true);
  });
  it("an approval waiting for calls says so, and its reply ends the wait", () => {
    const { container, press, ref } = setup();
    pause(300); press("Tab"); pause(300); press("Enter");
    const id = String(resolves()[0].request_id);
    act(() => { ref.current!.reply({ kind: "plan_waiting", requestId: id, tab: 1, calls: 2 }); });
    expect(container.querySelector(".plan-footer")?.textContent).toBe("waiting for 2 calls to finish");
    act(() => { ref.current!.reply({ kind: "command_result", requestId: id, ok: true }); });
    expect(container.querySelector(".plan-footer")?.textContent).toBe("Answer sent");
  });
  it("a refused plan shows why and offers no approval", () => {
    const { container } = setup({ plan: record("", { unsupportedReason: "The plan is longer than 256 KiB, so it was not shown" }) });
    expect(container.textContent).toContain("longer than 256 KiB");
    expect(container.querySelector<HTMLButtonElement>('[data-choice="approve_manual"]')!.disabled).toBe(true);
  });
  it("the arrival guard holds a click and a key", () => {
    let held = true;
    const { container, press } = setup({ blocked: () => held });
    pause(300); press("Tab"); pause(300); press("Enter");
    fireEvent.click(container.querySelector('[data-choice="approve_auto"]')!);
    expect(resolves()).toEqual([]);
    held = false;
    fireEvent.click(container.querySelector('[data-choice="approve_auto"]')!);
    expect(resolves()[0]).toMatchObject({ choice: "approve_auto" });
  });
});

it("action focus clamps at both ends for Tab and arrow keys", () => {
  const { container, press } = setup({ origin: "bypass" });
  for (const [key, props] of [["Tab", { shiftKey: true }], ["h", {}], ["ArrowLeft", {}]] as const) {
    press(key, props);
    expect(focused(container)).toContain("Keep planning");
  }
  for (let i = 0; i < 3; i += 1) press("Tab");
  for (const key of ["Tab", "l", "ArrowRight"]) {
    press(key);
    expect(focused(container)).toContain("Approve and keep Bypass");
  }
  expect(resolves()).toEqual([]);
});

it.each([{ key: "Tab" }, { key: "Tab", shiftKey: true }, { key: "Unidentified", shiftKey: true, code: "Tab" }])(
  "$key from feedback returns to the actions", (event) => {
    const { container, getByLabelText, press, dock } = setup({ draft: "keep this" });
    const field = getByLabelText("What to change in the plan (optional)") as HTMLTextAreaElement;
    act(() => field.focus());
    press(event.key, event, field);
    expect(document.activeElement).toBe(dock);
    expect(focused(container)).toContain("Keep planning");
    expect(field.value).toBe("keep this");
    expect(resolves()).toEqual([]);
  },
);

it("feedback is limited by UTF-8 bytes before mirroring or sending", () => {
  const { container, getByLabelText, press, unmount } = setup({ draft: "kept" });
  const field = getByLabelText("What to change in the plan (optional)") as HTMLTextAreaElement;
  const oversized = "中".repeat(5462);
  fireEvent.change(field, { target: { value: oversized } });
  pause(400);
  expect(container.querySelector(".plan-footer")?.textContent).toContain("16 KiB of UTF-8");
  expect(messages.filter((m) => m.type === "plan_feedback_draft")).toEqual([]);
  expect((container.querySelector(".plan-send") as HTMLButtonElement).disabled).toBe(true);
  press("Enter", { ctrlKey: true }, field);
  expect(resolves()).toEqual([]);
  expect(field.value).toBe(oversized);
  const fitting = "中".repeat(5461) + "a";
  fireEvent.change(field, { target: { value: fitting } });
  pause(400);
  expect(messages.filter((m) => m.type === "plan_feedback_draft")[0]).toMatchObject({ text: fitting });
  press("Enter", { ctrlKey: true }, field);
  expect(resolves()[0]).toMatchObject({ feedback: fitting });
  unmount();
});

it("an unsupported plan disables every action and explains how to end it", () => {
  const { container, press } = setup({ plan: record("", { unsupportedReason: "This plan cannot be shown" }) });
  expect([...container.querySelectorAll<HTMLButtonElement>(".plan-action")].every((b) => b.disabled)).toBe(true);
  expect(container.textContent).toContain("interrupt the turn to end it");
  pause(300); press("Enter");
  expect(container.querySelector(".plan-feedback")).toBeNull();
  expect(resolves()).toEqual([]);
});

it("a Mermaid fence in a plan stays code with no diagram marker", () => {
  const { container } = setup({ plan: record("```mermaid\nflowchart LR; A-->B\n```\n") });
  expect(container.querySelector("pre.code-block")?.textContent).toContain("A-->B");
  expect(container.querySelector("[data-code-diagram], img")).toBeNull();
});
