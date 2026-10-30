// The panel as the preview page shows it: the real app, fed a scripted conversation instead of
// Rust, with outgoing messages logged rather than delivered.
import { scenarioPackets, withAgent, withMode, type PreviewAgent, type Scenario, type ThinkingLevel } from "./scenario";
import { themeVars } from "./themes";

const params = new URLSearchParams(location.search);

window.webkit = {
  messageHandlers: { eitriAgent: { postMessage: (msg: string) => console.info("panel -> host", msg) } },
};

const theme = themeVars(params.get("theme") ?? "");
// `history=1`: the session was resumed, so the list opens with the restored-history notice.
const withHistory = (packets: unknown[]): unknown[] =>
  params.get("history") !== "1"
    ? packets
    : packets.map((p) => {
        const packet = p as { kind: string; state?: Record<string, unknown> };
        if (packet.kind !== "snapshot") return p;
        const history = {
          source: "claude_transcript", restoredItems: 12, omittedItems: 0, uptoSeq: 14,
          sourcePath: "/home/me/.claude/projects/-home-me-src-shop/9f2c.jsonl", attemptedTranscriptPath: null,
          fallbackReason: null, cliVersion: "2.1.0",
        };
        return { ...packet, state: { ...packet.state, history } };
      });
// `mode=manual|auto|bypass`: the tab runs in that mode (the scenarios script auto).
const mode = params.get("mode");
const withTabMode = (packets: unknown[]): unknown[] =>
  mode === "manual" || mode === "auto" || mode === "bypass" ? withMode(packets, mode) : packets;
// `restore=1`: the last window's tabs are on offer, one of them in bypass, as Rust sends them in `hello` at launch.
const withRestore = (packets: unknown[]): unknown[] =>
  params.get("restore") !== "1"
    ? packets
    : packets.map((p) => {
        const packet = p as { kind: string };
        return packet.kind === "hello"
          ? { ...packet, restore: { labels: ["登录页窄屏", "checkout webhook", "docs", "e2e"], bypass: 1 } }
          : p;
      });
// `tab=starting|failed`: the active tab is starting its agent, or its start failed (the start screen's other two faces).
const tabState = params.get("tab");
const withTabState = (packets: unknown[]): unknown[] =>
  tabState !== "starting" && tabState !== "failed"
    ? packets
    : packets.map((p) => {
        const packet = p as { kind: string; tabs?: Record<string, unknown>[] };
        if (packet.kind !== "tabs" || packet.tabs === undefined) return p;
        const failure = tabState === "failed" ? "claude exited before the session started (exit status 1)" : null;
        return { ...packet, tabs: packet.tabs.map((tab) => ({ ...tab, state: tabState, failure })) };
      });
const packets = withTabState(withRestore(withHistory(withTabMode(withAgent(
  scenarioPackets(
    (params.get("scenario") ?? "running") as Scenario,
    theme,
    (params.get("thinking") ?? "collapsed") as ThinkingLevel,
  ),
  (params.get("agent") ?? "claude") as PreviewAgent,
)))));
// A draft typed into the box goes in the way Rust's own copy of a draft does (a `draft` envelope for the
// active tab), then INPUT is entered so the menu opens as it would for a user who had typed it.
const draft = params.get("draft");
function feed() {
  if (!window.__eitriDispatch) {
    setTimeout(feed, 20);
    return;
  }
  for (const p of packets) window.__eitriDispatch(JSON.stringify(p));
  if (draft) window.__eitriDispatch(JSON.stringify({ kind: "draft", tab: 1, text: draft }));
  // `notice=TEXT` is a line Rust says once, which the band shows as its flash.
  const notice = params.get("notice");
  if (notice) window.__eitriDispatch(JSON.stringify({ kind: "notice", text: notice }));
  if (params.get("input") === "1" || draft) window.__eitriDispatch(JSON.stringify({ kind: "enter_input" }));
  // `focused=0`: the keys are elsewhere in the window (the editor), as at launch.
  if (params.get("focused") === "0") window.__eitriDispatch(JSON.stringify({ kind: "pane_focus", focused: false }));
  // `companion=1`: the panel is its own window and no editor has attached yet.
  if (params.get("companion") === "1") window.__eitriDispatch(JSON.stringify({ kind: "editor_link", state: "none", text: "" }));
  // `esc=1` presses Esc once things have landed: the start screen opens in INSERT, and this shows its BROWSE.
  if (params.get("esc") === "1") {
    setTimeout(() => (document.activeElement ?? document.body).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })), 300);
  }
  // `keys=k,k` presses those keys one by one once `esc` has landed: on the start screen this walks its cursor.
  const keys = (params.get("keys") ?? "").split(",").filter((key) => key !== "");
  keys.forEach((key, i) => {
    setTimeout(
      () => (document.activeElement ?? document.body).dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true })),
      600 + i * 120,
    );
  });
  // `cursor=N` moves the conversation cursor N rows up from the last row, as N presses of `k` would.
  const up = Number(params.get("cursor") ?? "0");
  // Spaced out, so each press is its own step.
  for (let i = 0; i < up; i += 1) {
    setTimeout(() => document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "k", bubbles: true })), 400 + i * 120);
  }
  // `open=1` opens every row's detail, as `Ctrl+o` does, after the cursor has moved.
  if (params.get("open") === "1") {
    setTimeout(
      () => document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "o", ctrlKey: true, bubbles: true })),
      500 + up * 120,
    );
  }
}
void import("../main").then(feed);
