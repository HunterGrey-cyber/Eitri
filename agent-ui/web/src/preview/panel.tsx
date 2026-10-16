// The panel as the preview page shows it: the real app, fed a scripted conversation instead of
// Rust, with outgoing messages logged rather than delivered.
import { scenarioPackets, withAgent, type PreviewAgent, type Scenario, type ThinkingLevel } from "./scenario";
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
const packets = withHistory(withAgent(
  scenarioPackets(
    (params.get("scenario") ?? "running") as Scenario,
    theme,
    (params.get("thinking") ?? "collapsed") as ThinkingLevel,
  ),
  (params.get("agent") ?? "claude") as PreviewAgent,
));
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
  if (params.get("input") === "1" || draft) window.__eitriDispatch(JSON.stringify({ kind: "enter_input" }));
  // `esc=1` presses Esc once things have landed: the start screen opens in INSERT, and this shows its BROWSE.
  if (params.get("esc") === "1") {
    setTimeout(() => (document.activeElement ?? document.body).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })), 300);
  }
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
