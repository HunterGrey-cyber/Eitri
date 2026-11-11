// @vitest-environment jsdom
/**
 * The phone's transcript page: the message list alone, fed frames through a transport and posting back only
 * `ready`, `more`, `open_link` and `want_snapshot`.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { initialState } from "../reducer";
import type { Transport } from "../transport";
import { TranscriptApp } from "./TranscriptApp";

afterEach(() => {
  cleanup();
  document.documentElement.removeAttribute("style");
});

const key = "k-0123456789abcdef";
const location = "shop · window 1 · tab 2";
const hello = { kind: "host", protocol: 1, deskProtocol: 1, app: "Eitri", device: "dev-phone", location };
const theme = { vars: { "--nv-bg": "#112233", "--nv-warn-tint": "#332211" }, scheme: "dark" };
const greeting = (over: Record<string, unknown> = {}) => ({
  v: 1, kind: "greeting", key,
  greeting: { tab: {}, prompt: null, root_note: null, notices: [], shell_running: false, theme, send_block: null, ...over },
});
const snapshot = (state: Record<string, unknown> = {}, over: Record<string, unknown> = {}) => ({
  v: 1, kind: "snapshot", key, generation: 1, through: 20, first_turn: 2, cut: [],
  frame: { kind: "snapshot", state: {
    ...initialState(),
    userPrompts: [{ seq: 10, text: "two" }, { seq: 15, text: "three" }],
    transcript: [{ seq: 12, text: "a reply" }, { seq: 17, text: "latest" }],
    ...state,
  } },
  ...over,
});

function fakeTransport() {
  const posted: Record<string, unknown>[] = [];
  let deliver: ((json: string) => void) | null = null;
  const transport: Transport = {
    name: "android",
    post: (json) => posted.push(JSON.parse(json)),
    listen: (onMessage) => { deliver = onMessage; },
    nextId: (kind) => `a-${kind}-1`,
  };
  const send = (...frames: unknown[]) => act(() => {
    for (const frame of frames) deliver?.(JSON.stringify(frame));
  });
  return { transport, posted, send, kinds: () => posted.map((p) => p.kind) };
}

function setup(...frames: unknown[]) {
  const t = fakeTransport();
  const view = render(<TranscriptApp transport={t.transport} />);
  t.send(hello, greeting(), ...frames);
  const list = () => view.container.querySelector<HTMLElement>(".message-list")!;
  return { ...t, ...view, list };
}

describe("the transcript page", () => {
  it("renders_only_the_message_list", () => {
    const history = { source: "claude_transcript", restoredItems: 4, omittedItems: 0, uptoSeq: 9,
      sourcePath: "/desk/.claude/projects/x.jsonl", attemptedTranscriptPath: null, fallbackReason: null, writerVersion: null };
    const { container, kinds } = setup(snapshot({ history }));
    expect(container.querySelector(".history-notice")).not.toBeNull();
    expect(container.querySelectorAll("button")).toHaveLength(0);
    expect(kinds()[0]).toBe("ready");
    expect(container.querySelector(".message-list")).not.toBeNull();
    expect(container.textContent).toContain("latest");
    for (const desk of [".tab-bar", ".status-band", ".band-seg", ".composer", ".activity-line", ".queue-lines",
      ".cursor-band", ".visual-paint", "textarea", "input"]) {
      expect(container.querySelector(desk), desk).toBeNull();
    }
    expect(container.querySelector(".row-current")).toBeNull();
  });

  it("card_has_no_controls_and_names_the_desk", () => {
    const { container } = setup(snapshot({
      pendingPermissions: [{ seq: 18, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    }));
    const card = container.querySelector<HTMLElement>(".permission-card")!;
    expect(card).not.toBeNull();
    expect(card.textContent).toContain("cargo fmt");
    expect(card.querySelectorAll("button, input, textarea")).toHaveLength(0);
    const line = card.querySelector<HTMLElement>(".answer-at-desk")!;
    expect(line.textContent).toBe(`Answer at the desk: ${location}. Stop on this phone ends the turn and denies this card.`);
    expect(line.querySelector("strong")?.textContent).toBe("Answer at the desk:");
  });

  it("question_and_trust_prompt_have_no_controls", () => {
    const question = {
      seq: 18, interactionId: "q1", toolUseId: null, turnId: null, unsupportedReason: null, state: { kind: "waiting" },
      questions: [{ question: "Which colour?", header: "Colour", multiSelect: false,
        options: [{ label: "Red", description: "warm", preview: null }, { label: "Blue", description: "cool", preview: null }] }],
    };
    const t = setup(snapshot({ questions: [question] }));
    const dock = t.container.querySelector<HTMLElement>(".question-at-desk")!;
    expect(dock.textContent).toContain("Which colour?");
    expect(dock.textContent).toContain("Red");
    expect(dock.textContent).toContain("cool");
    expect(dock.querySelector(".answer-at-desk")?.textContent).toContain(`Answer at the desk: ${location}.`);
    expect(dock.querySelectorAll("button, input, textarea")).toHaveLength(0);

    t.send(greeting({ prompt: { kind: "trust" } }));
    const trust = t.container.querySelector<HTMLElement>(".prompt-at-desk")!;
    expect(trust.textContent).toContain("trust");
    expect(trust.querySelector(".answer-at-desk")?.textContent).toBe(`Answer at the desk: ${location}.`);
    expect(trust.querySelectorAll("button, input, textarea")).toHaveLength(0);

    t.send(greeting({ prompt: { kind: "bypass", lines: ["Bypass skips every card.", "Enter bypass?"] } }));
    const bypass = t.container.querySelector<HTMLElement>(".prompt-at-desk")!;
    expect(bypass.textContent).toContain("Bypass skips every card.");
    expect(bypass.querySelectorAll("button, input, textarea")).toHaveLength(0);
    expect(t.container.querySelectorAll("button, input, textarea")).toHaveLength(0);
  });

  it("greeting_notices_and_root_note_shown", () => {
    const t = setup();
    t.send(greeting({ notices: ["API retry in 5s"], root_note: "The project folder is no boundary", shell_running: true }),
      snapshot());
    const text = t.container.textContent ?? "";
    expect(text).toContain("API retry in 5s");
    expect(text).toContain("The project folder is no boundary");
    expect(t.container.querySelector(".shell-running")?.textContent).toContain("!");
  });

  it("top_reached_posts_more_once", () => {
    const t = setup(snapshot());
    const list = t.list();
    list.scrollTop = 0;
    fireEvent.scroll(list);
    fireEvent.scroll(list);
    const more = t.posted.filter((p) => p.kind === "more");
    expect(more).toEqual([{ kind: "more", key, generation: 1, before_turn: 2 }]);
  });

  it("cut_entry_loads_whole_on_tap", () => {
    const t = setup(snapshot({ transcript: [{ seq: 12, text: "start of a large reply" }, { seq: 17, text: "latest" }] },
      { cut: [{ entry_id: "transcript:12", bytes: 2_097_152 }] }));
    // The page asks for older turns first; the cut entry waits for that page.
    t.send({ v: 1, kind: "page", key, generation: 1, from_turn: 0, through_turn: 1,
      frame: { kind: "snapshot", state: { ...initialState(), userPrompts: [{ seq: 1, text: "zero" }, { seq: 5, text: "one" }],
        transcript: [] } } });
    t.posted.length = 0;
    const cut = t.container.querySelector<HTMLElement>(".cut-entry")!;
    expect(cut.textContent).toContain("2.0 MB");
    fireEvent.click(cut);
    expect(t.posted).toEqual([{ kind: "more", key, generation: 1, before_turn: 3 }]);
    fireEvent.click(cut);
    expect(t.posted).toHaveLength(1);
    t.send({ v: 1, kind: "page", key, generation: 1, from_turn: 2, through_turn: 2,
      frame: { kind: "snapshot", state: { ...initialState(), userPrompts: [{ seq: 10, text: "two" }],
        transcript: [{ seq: 12, text: "the whole large reply" }] } } });
    expect(t.container.querySelector(".cut-entry")).toBeNull();
    expect(t.container.textContent).toContain("the whole large reply");
  });

  it("a_cut_entry_tapped_while_a_page_is_owed_is_asked_for_when_it_arrives", () => {
    const t = setup(snapshot({ transcript: [{ seq: 12, text: "start of a large reply" }, { seq: 17, text: "latest" }] },
      { cut: [{ entry_id: "transcript:12", bytes: 2_097_152 }] }));
    const list = t.list();
    list.scrollTop = 0;
    fireEvent.scroll(list);
    expect(t.posted.filter((p) => p.kind === "more")).toEqual([{ kind: "more", key, generation: 1, before_turn: 2 }]);
    t.posted.length = 0;
    fireEvent.click(t.container.querySelector<HTMLElement>(".cut-entry")!);
    expect(t.posted).toEqual([]);
    expect(t.container.querySelector(".cut-entry")?.textContent).toContain("loads once the older turns arrive");
    t.send({ v: 1, kind: "page", key, generation: 1, from_turn: 0, through_turn: 1,
      frame: { kind: "snapshot", state: { ...initialState(), userPrompts: [{ seq: 1, text: "zero" }, { seq: 5, text: "one" }],
        transcript: [] } } });
    expect(t.posted).toEqual([{ kind: "more", key, generation: 1, before_turn: 3 }]);
  });

  it.each(["the session changed", "not loaded"])("a_refused_more_is_remembered_until_a_fresh_snapshot: %s", (why) => {
    const t = setup(snapshot());
    const list = t.list();
    list.scrollTop = 0;
    fireEvent.scroll(list);
    const shown = t.container.textContent;
    t.send({ v: 1, kind: "refused", key, why });
    expect(t.container.textContent).toBe(shown);
    fireEvent.scroll(list);
    expect(t.posted.filter((p) => p.kind === "more")).toHaveLength(1);
    // The same session again, with the same turns: the refusal was about the moment, not the request.
    t.send(snapshot());
    fireEvent.scroll(list);
    expect(t.posted.filter((p) => p.kind === "more")).toEqual([
      { kind: "more", key, generation: 1, before_turn: 2 },
      { kind: "more", key, generation: 1, before_turn: 2 },
    ]);
  });

  it("an_opened_run_folds_again_by_a_tap", () => {
    const call = (seq: number, id: string) => ({ seq, toolUseId: id, name: "Read", input: { file_path: `/p/${id}` },
      result: { content: "x", isError: false }, turnId: "turn-1" });
    const t = setup(snapshot({ toolCalls: [call(13, "tu-1"), call(14, "tu-2"), call(16, "tu-3")] }));
    const run = () => t.container.querySelector<HTMLElement>(".row-tool-run");
    expect(run()).not.toBeNull();
    fireEvent.click(run()!);
    expect(run()).toBeNull();
    const fold = t.container.querySelector<HTMLElement>(".fold-run")!;
    expect(fold.textContent).toContain("tap to fold");
    fireEvent.click(fold);
    expect(run()).not.toBeNull();
    expect(t.container.querySelector(".fold-run")).toBeNull();
  });

  it("link_tap_posts_open_link_and_navigates_nowhere", () => {
    const t = setup(snapshot({ transcript: [{ seq: 12, text: "see [docs](https://example.com/a) and [here](notes.md)" }] }));
    t.posted.length = 0;
    const [web, relative] = Array.from(t.container.querySelectorAll<HTMLAnchorElement>(".message-list a"));
    expect(fireEvent.click(web)).toBe(false);
    expect(t.posted).toEqual([{ kind: "open_link", url: "https://example.com/a" }]);
    expect(fireEvent.click(relative)).toBe(false);
    expect(t.posted).toHaveLength(1);
  });

  it("sent_from_this_phone_and_from_another", () => {
    const t = setup(snapshot({ sentFrom: {
      "10": { deviceId: "dev-phone", name: "Pixel 9" },
      "15": { deviceId: "dev-tablet", name: "Tab S9" },
    } }));
    const sources = Array.from(t.container.querySelectorAll(".phone-source")).map((el) => el.textContent);
    expect(sources).toEqual(["sent from this phone", "from Tab S9"]);
  });

  it("stopped_from_shown_on_the_footer", () => {
    const t = setup(snapshot({
      turnEndings: [{ seq: 18, turnId: "turn-1", kind: "interrupted", reason: null, apiErrorStatus: null, message: null }],
      stoppedFrom: { "turn-1": { deviceId: "dev-tablet", name: "Tab S9" } },
    }));
    expect(t.container.querySelector(".turn-time")?.textContent).toContain("stopped from Tab S9");
    t.send(snapshot({
      turnEndings: [{ seq: 18, turnId: "turn-1", kind: "interrupted", reason: null, apiErrorStatus: null, message: null }],
      stoppedFrom: { "turn-1": { deviceId: "dev-phone", name: "Pixel 9" } },
    }));
    const footer = t.container.querySelector(".turn-time")?.textContent ?? "";
    expect(footer).toContain("stopped from this phone");
    expect(footer).not.toContain("Pixel 9");
  });

  it("update_row_shown_once", () => {
    const t = setup(snapshot());
    t.send({ v: 1, kind: "future" }, { v: 1, kind: "another_future" });
    const rows = t.container.querySelectorAll(".update-row");
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain("update the app");
    expect(t.container.textContent).toContain("latest");
  });

  it("theme_tokens_become_css_variables", () => {
    const t = fakeTransport();
    render(<TranscriptApp transport={t.transport} />);
    const style = document.documentElement.style;
    // Before the desk's colours arrive, the built-in dark pair (jsdom reports no light preference).
    expect(style.getPropertyValue("--nv-bg")).toBe("#1a1b26");
    expect(style.getPropertyValue("--nv-font-prose")).toBe("");
    t.send(hello, greeting());
    expect(style.getPropertyValue("--nv-bg")).toBe("#112233");
    expect(style.getPropertyValue("--nv-warn-tint")).toBe("#332211");
    expect(style.getPropertyValue("--nv-color-scheme")).toBe("dark");
    t.send({ v: 1, kind: "theme", theme: { vars: { "--nv-bg": "#eeeeee" }, scheme: "light" } });
    expect(style.getPropertyValue("--nv-bg")).toBe("#eeeeee");
    expect(style.getPropertyValue("--nv-color-scheme")).toBe("light");
  });

  it("a_gap_posts_want_snapshot_once", () => {
    const t = setup(snapshot());
    t.send({ v: 1, kind: "events", key, generation: 1, from: 25, through: 26, frame: { kind: "events", events: [] } },
      { v: 1, kind: "events", key, generation: 1, from: 26, through: 27, frame: { kind: "events", events: [] } });
    expect(t.posted.filter((p) => p.kind === "want_snapshot")).toEqual([{ kind: "want_snapshot", key }]);
    t.send(snapshot(), { v: 1, kind: "events", key, generation: 1, from: 30, through: 31, frame: { kind: "events", events: [] } });
    expect(t.posted.filter((p) => p.kind === "want_snapshot")).toHaveLength(2);
  });

  it("tap_toggles_a_tool_row_and_the_fold_says_tap", () => {
    const output = Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n");
    const t = setup(snapshot({ toolCalls: [{ seq: 13, toolUseId: "tu-1", name: "Bash", input: { command: "ls" },
      result: { content: output, isError: false } }] }));
    const more = t.container.querySelector<HTMLElement>(".tool-result-preview-more")!;
    expect(more.textContent).toMatch(/^… \+\d+ lines · tap to show$/);
    fireEvent.click(more);
    expect(t.container.querySelector(".tool-result-preview-more")).toBeNull();
    expect(t.container.textContent).toContain("line 29");
  });

  it("older_turns_keep_the_place_by_the_page_alone", () => {
    // The list moves itself down by what was prepended; the engine's own scroll anchoring would move it again.
    const pageCss = readFileSync(join(__dirname, "transcript.css"), "utf8");
    expect(pageCss).toMatch(/\.transcript-app \.message-list \{[^}]*overflow-anchor: none;/);
  });

  it("only_the_four_kinds_are_posted", () => {
    const t = setup(snapshot({ transcript: [{ seq: 12, text: "[x](https://example.com)" }] }));
    t.send({ v: 1, kind: "events", key, generation: 1, from: 99, through: 100, frame: { kind: "events", events: [] } });
    fireEvent.scroll(t.list());
    fireEvent.click(t.container.querySelector("a")!);
    expect(new Set(t.kinds())).toEqual(new Set(["ready", "more", "open_link", "want_snapshot"]));
  });
});
