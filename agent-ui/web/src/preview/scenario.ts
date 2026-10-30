// A scripted conversation for the preview page: the envelopes Rust would send, in order.
import { EMPTY_PANEL_TABLE } from "../keymap";

const cwd = "/home/me/src/shop";

const prose1 = `问题在 \`LoginForm\` 的按钮行：它用了固定的 \`min-width: 320px\`，面板窄于这个宽度时整行被撑出容器。

我把按钮行改成可以换行，并让按钮在窄屏下占满一行。`;

const prose2 = `### 改好了

窄屏下按钮行现在会换行，测试也全部通过。

- \`LoginForm.css\`：按钮行去掉固定最小宽度，改用 \`flex-wrap\`
- 宽度小于 \`420px\` 时，两个按钮各占一整行
- 新增一条快照测试，覆盖 360px 宽的情况

\`\`\`css
.login-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
\`\`\`

如果你希望窄屏下"忘记密码"链接也移到按钮下面，我可以接着改。`;

const prose3 = `我先跑一遍完整的端到端测试，确认登录流程在三个断点下都正常。`;

const editOld = `.login-actions {
  display: flex;
  min-width: 320px;
  gap: 8px;
}`;
const editNew = `.login-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

@media (max-width: 420px) {
  .login-actions > button { flex: 1 1 100%; }
}`;

const prose4 = `三个断点的对比如下：

| 断点 | 截图差异 | 说明 |
|---|---|---|
| 360px | 无 | 按钮各占一行 |
| 768px | 2 处 | 链接位置下移 4px |
| 1280px | 无 | 与之前一致 |

768px 的差异来自新加的 \`gap\`，属于预期变化。我已经新建了截图基线文件。`;

const writeContent = `import { test, expect } from "@playwright/test";

for (const width of [360, 768, 1280]) {
  test(\`login at \${width}px\`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    await page.goto("/login");
    await expect(page).toHaveScreenshot(\`login-\${width}.png\`);
  });
}
`;

// What the agent thought, as plain text the way a provider streams it (no markdown is rendered).
const thinking1 = `The user says the buttons overflow on narrow screens.
Likely a fixed width somewhere in the login form's button row.
Search the stylesheet for the class before reading whole files.
先 grep 一下 login-actions，再看组件和样式。`;
const thinking2 = `LoginForm.tsx only renders the row; the width comes from the CSS.`;
const thinking3 = `Found it: \`min-width: 320px\` on .login-actions.
The panel can be narrower than 320px, so the row is pushed out of its container.
Options:
1. drop the min-width and let the row wrap
2. stack the buttons below 420px
Both together keep the wide layout unchanged.
改动只在 CSS 里，不用动组件。`;
const thinking4 = `The user wants the 4px shift at 768px gone.
It comes from the new gap on .login-actions, which also applies at 768px.
The link sits in the same flex row, so the gap pushes it down when the row wraps.
Could scope the gap to the narrow breakpoint only,
or move the link out of the row.
先看一下 768px 下这一行到底有没有换行……`;

/** A finished turn's duration as Rust sends it (`turnTimes`): `anchorSeq` is the row it ended on. */
function turnTime(turnId: string, anchorSeq: number | null, durationMs: number) {
  const startedAtMs = 1_760_000_000_000;
  return { turnId, startedAtMs, endedAtMs: startedAtMs + durationMs, anchorSeq };
}

export type Scenario = "running" | "permission" | "done" | "long" | "empty" | "empty-none";
export type ThinkingLevel = "collapsed" | "preview" | "expanded" | "hidden";

/** Whose tab the page shows, for the `/` menu: a Claude tab (the built-in table), an ACP tab that listed
 *  commands (`availableCommands`, as `commands_available` leaves them), or a Codex tab (no menu). */
export type PreviewAgent = "claude" | "acp" | "codex";

const ACP_COMMANDS = [
  { name: "web", description: "Search the web for information", hint: "query to search for" },
  { name: "test", description: "Run tests for the current project" },
  { name: "plan", description: "Create a detailed implementation plan", hint: "description of what to plan" },
  { name: "review", description: "Review the changes on this branch" },
];

/** The lines above the box in the running scenario, as Rust's queue envelope carries them. Codex takes a
 *  message into the running turn (`↪` until it reads it), and one it refused waits for the turn's end
 *  (`⧗`, with the reason once); Claude and an ACP agent cannot take one yet, so both wait. */
function runningQueue(agent: PreviewAgent) {
  const steers = agent === "codex";
  return {
    kind: "queue",
    tab: 1,
    items: [
      { text: "顺便看看 768px 那 4px 能不能去掉", queuedAt: 1, steered: steers },
      { text: "跑完之后把 README 里的截图也更新一下", queuedAt: 2, steered: false },
    ],
    error: null,
    note: steers
      ? "held until the turn ends: Codex did not take it: the active turn is not steerable"
      : `held until the turn ends: ${agent === "acp" ? "demo" : "Claude"} cannot take messages mid-turn`,
  };
}

/** The same scenario as `agent`'s tab: the tab's `agent`, the state's `availableCommands` and whether Enter
 *  steers (`capabilities.steer`, which Codex advertises) differ, and the running scenario's queue with it. */
export function withAgent(packets: unknown[], agent: PreviewAgent): unknown[] {
  return (packets as { kind: string; tabs?: Record<string, unknown>[]; state?: Record<string, unknown>; items?: unknown[] }[]).map((p) => {
    if (p.kind === "tabs" && agent !== "claude") {
      return { ...p, tabs: p.tabs!.map((t) => ({ ...t, agent: agent === "acp" ? "acp:demo" : "codex" })) };
    }
    if (p.kind === "snapshot") {
      const capabilities = { ...(p.state!.capabilities as Record<string, unknown>), steer: agent === "codex" };
      const availableCommands = agent === "acp" ? ACP_COMMANDS : agent === "codex" ? null : p.state!.availableCommands;
      return { ...p, state: { ...p.state, availableCommands, capabilities } };
    }
    if (p.kind === "queue" && p.items!.length > 0) return runningQueue(agent);
    return p;
  });
}

/** The same scenario with the tab in `mode`, and the handshake offering all three modes (the scripted
 *  Claude scenarios offer two): what the page shows for a manual tab. */
export function withMode(packets: unknown[], mode: "manual" | "auto" | "bypass"): unknown[] {
  return (packets as { kind: string; tabs?: Record<string, unknown>[] }[]).map((p) => {
    if (p.kind === "tabs") return { ...p, tabs: p.tabs!.map((t) => ({ ...t, mode })) };
    if (p.kind === "hello") return { ...p, permissionModes: ["manual", "auto", "bypass"] };
    return p;
  });
}

/** The items of a scripted snapshot carry `seq`s written with gaps filled by fractions (a thought slotted
 *  in between two items); this gives every item the integer of its place in the one order, as Rust's
 *  numbering would, and moves the snapshot's revision past them. */
function renumber(snapshot: { throughRevision?: number; state?: Record<string, unknown>; turnTimes?: { anchorSeq: number | null }[] }): void {
  const st = snapshot.state as Record<string, { seq: number }[] | unknown>;
  const lists = ["userPrompts", "transcript", "toolCalls", "pendingPermissions", "turnEndings", "thinking"];
  const items = lists.flatMap((name) => (Array.isArray(st[name]) ? (st[name] as { seq: number }[]) : []));
  // `turnTimes` names the row a turn ended on by its `seq`, so it follows the renumbering.
  const placed = new Map(items.map((item) => [item.seq, item]));
  const anchors = (snapshot.turnTimes ?? []).map((time) => ({ time, row: time.anchorSeq === null ? undefined : placed.get(time.anchorSeq) }));
  items.sort((a, b) => a.seq - b.seq).forEach((item, at) => (item.seq = at));
  for (const { time, row } of anchors) if (row !== undefined) time.anchorSeq = row.seq;
  st.nextSeq = items.length;
  snapshot.throughRevision = items.length;
}

export function scenarioPackets(scenario: Scenario, theme: Record<string, string>, level: ThinkingLevel = "collapsed"): unknown[] {
  if (scenario === "empty" || scenario === "empty-none") return emptyPackets(theme, scenario === "empty");
  const packets = scenario === "long" ? longPackets(theme) : basePackets(scenario, theme);
  renumber(packets.find((p) => (p as { kind: string }).kind === "snapshot") as { state: Record<string, unknown> });
  // How thinking is shown reaches the page as the host sends it: a `display` envelope after `hello`.
  const hello = packets.findIndex((p) => (p as { kind: string }).kind === "hello");
  packets.splice(hello + 1, 0, { kind: "display", thinking: level });
  return packets;
}

function basePackets(scenario: Scenario, theme: Record<string, string>): unknown[] {
  const running = scenario !== "done";
  const toolCalls: unknown[] = [
    { seq: 3, toolUseId: "grep-1", name: "Grep", turnId: "t1",
      input: { pattern: "login-actions", path: `${cwd}/src` },
      result: { content: "src/components/LoginForm.css:12\nsrc/components/LoginForm.tsx:48", isError: false } },
    { seq: 4, toolUseId: "read-1", name: "Read", turnId: "t1",
      input: { file_path: `${cwd}/src/components/LoginForm.tsx` },
      result: { content: "export function LoginForm() { ... }", isError: false } },
    { seq: 5, toolUseId: "read-2", name: "Read", turnId: "t1",
      input: { file_path: `${cwd}/src/components/LoginForm.css` },
      result: { content: ".login-actions { ... }", isError: false } },
    { seq: 7, toolUseId: "edit-1", name: "Edit", turnId: "t1",
      input: { file_path: `${cwd}/src/components/LoginForm.css`, old_string: editOld, new_string: editNew },
      result: { content: "The file has been updated.", isError: false } },
    { seq: 8, toolUseId: "bash-1", name: "Bash", turnId: "t1",
      input: { command: "npm test -- LoginForm", description: "Run the LoginForm tests" },
      result: { content: "FAIL src/components/LoginForm.test.tsx\n  ✕ matches the 360px snapshot (24 ms)\n\n  Snapshot name: `LoginForm narrow 1`\n  - Snapshot  - 1\n  + Received  + 1", isError: true } },
    { seq: 9, toolUseId: "bash-2", name: "Bash", turnId: "t1",
      input: { command: "npm test -- LoginForm -u", description: "Update the snapshot and rerun" },
      result: { content: "PASS src/components/LoginForm.test.tsx\n  ✓ renders (31 ms)\n  ✓ matches the 360px snapshot (12 ms)\n\nTests: 2 passed, 2 total", isError: false } },
  ];
  if (running) {
    toolCalls.push({ seq: 13, toolUseId: "bash-3", name: "Bash", turnId: "t2",
      input: { command: "npx playwright test e2e/login.spec.ts", description: "Run the login end-to-end tests" },
      result: null });
  }
  const state = {
    backend: "legacy", conversationId: "demo", sessionId: "demo", providerSessionId: "demo",
    model: "claude-opus-5-5", cwd,
    userPrompts: [
      { seq: 1, text: "登录页在窄屏下按钮被挤出去了，帮我看看" },
      ...(running ? [{ seq: 11, text: "好的，顺便把端到端测试也跑一下，三个断点都要看。\n跑完告诉我有没有截图差异。" }] : []),
    ],
    transcript: [
      { seq: 2, text: "我先找一下按钮行的样式在哪儿定义。" },
      { seq: 6, text: prose1 },
      { seq: 10, text: prose2 },
      ...(running ? [{ seq: 12, text: prose3 }] : []),
    ],
    toolCalls,
    status: { kind: running ? "running" : "idle" },
    activeTurnId: running ? "t2" : null,
    pendingPermissions: scenario === "permission"
      ? [{ seq: 14, permissionId: "p1", toolUseId: "bash-3", toolName: "Bash",
          input: { command: "npx playwright test e2e/login.spec.ts", description: "Run the login end-to-end tests" } }]
      : [],
    capabilities: { resume: false, fork: false, interrupt: true, bypassPermissionMode: false },
    provider: null, usage: null, accountLimits: null, backgroundCalls: 0, turnEndings: [], lastTurnEnding: null,
    availableCommands: null,
    // Before the first reply, between the reads (folded into their run until it is opened), and before
    // the explanation.
    thinking: [
      { seq: 1.5, turnId: "t1", text: thinking1 },
      { seq: 4.5, turnId: "t1", text: thinking2 },
      { seq: 5.5, turnId: "t1", text: thinking3 },
    ],
    thinkingOpen: false,
    turnStarts: [], notices: [], questions: [],
    history: null, assistantMessageOpen: false, nextSeq: 15, turnThinking: false,
    denialsBeforeTheirCall: [], changedInputsBeforeTheirCall: [],
  };
  const tab = { id: 1, number: 1, label: "登录页窄屏", name: null, state: "live", mode: "auto", marker: null,
    pending: 0, resumable: false, failure: null, title: "登录页窄屏" };
  return [
    { kind: "theme", vars: theme },
    { kind: "hello", backend: "legacy", projectDir: cwd, permissionModes: ["auto", "bypass"],
      resumableSessions: [], expectedVerdandiRevision: null, account: null },
    { kind: "tabs", active: 1, tabs: [tab] },
    { kind: "snapshot", tab: 1, throughRevision: 15, state,
      turnTimes: [turnTime("t1", 10, 42_000)] },
    { kind: "pane_focus", focused: true },
    running ? runningQueue("claude") : { kind: "queue", tab: 1, items: [], error: null, note: null },
  ];
}

/** A longer session: three turns, a table, a new file, an interrupted turn, and the agent thinking. */
function longPackets(theme: Record<string, string>): unknown[] {
  const base = basePackets("done", theme) as { kind: string; state?: Record<string, unknown> }[];
  const snap = base.find((p) => p.kind === "snapshot")!;
  const st = snap.state as Record<string, unknown[] | unknown>;
  (st.userPrompts as unknown[]).push(
    { seq: 11, text: "把端到端测试也跑一下，三个断点都要看，跑完告诉我有没有截图差异。" },
    { seq: 20, text: "再把 README 里的截图更新一下" },
    { seq: 25, text: "算了先不更新 README，帮我看看 768px 那 4px 能不能去掉？" },
  );
  (st.transcript as unknown[]).push(
    { seq: 12, text: "仓库里还没有登录页的端到端测试，我先建一个。" },
    { seq: 17, text: prose4 },
    { seq: 21, text: "好的，我先找一下 README 里引用截图的位置。" },
  );
  (st.toolCalls as unknown[]).push(
    { seq: 13, toolUseId: "glob-1", name: "Glob", turnId: "t2", input: { pattern: "e2e/**/*.spec.ts" },
      result: { content: "e2e/home.spec.ts\ne2e/cart.spec.ts", isError: false } },
    { seq: 14, toolUseId: "ws-1", name: "WebSearch", turnId: "t2", input: { query: "playwright toHaveScreenshot viewport" },
      result: { content: "Visual comparisons | Playwright", isError: false } },
    { seq: 15, toolUseId: "write-1", name: "Write", turnId: "t2", createsFile: true,
      input: { file_path: "/home/me/src/shop/e2e/login.spec.ts", content: writeContent },
      result: { content: "File created successfully", isError: false } },
    { seq: 16, toolUseId: "bash-4", name: "Bash", turnId: "t2",
      input: { command: "npx playwright test e2e/login.spec.ts --update-snapshots", description: "Create the screenshot baselines" },
      result: { content: "Running 3 tests using 3 workers\n  ✓  1 login at 360px (1.2s)\n  ✓  2 login at 768px (1.3s)\n  ✓  3 login at 1280px (1.1s)\n\n  3 passed (3.4s)", isError: false } },
    { seq: 22, toolUseId: "grep-2", name: "Grep", turnId: "t3", input: { pattern: "login.*\\.png", path: "/home/me/src/shop/README.md" },
      result: { content: "README.md:41", isError: false } },
  );
  (st.turnEndings as unknown[]).push({ seq: 24, turnId: "t3", kind: "interrupted", reason: null, apiErrorStatus: null, message: null });
  // t1 and t2 completed (a footer each, under prose2 and the table); the interrupted t3 says how long on its row.
  (snap as { turnTimes?: unknown[] }).turnTimes = [
    turnTime("t1", 10, 42_000),
    turnTime("t2", 17, 65_000),
    turnTime("t3", 22, 12_000),
  ];
  // The running turn is still thinking: an open block, streaming.
  (st.thinking as unknown[]).push({ seq: 26, turnId: "t4", text: thinking4 });
  st.thinkingOpen = true;
  st.status = { kind: "running" };
  st.activeTurnId = "t4";
  st.turnThinking = true;
  st.nextSeq = 40;
  (snap as { throughRevision?: number }).throughRevision = 40;
  return base;
}

/** The start screen: a tab with no session yet. With `sessions`, the project has earlier sessions to resume
 *  and the last window's tabs to bring back; without, it is a fresh project. */
function emptyPackets(theme: Record<string, string>, sessions: boolean): unknown[] {
  const tab = { id: 1, number: 1, label: "1", name: null, state: "not_started", mode: "auto", marker: null,
    pending: 0, resumable: false, failure: null, title: null };
  // Stamped against the page's own clock, so the start screen's "12 min ago" reads as it would on the day.
  const record = (id: string, title: string, minutesAgo: number) => {
    const at = String(Date.now() - minutesAgo * 60_000);
    return { provider: "claude", providerSessionId: id, createdAt: at, updatedAt: at, title, name: null };
  };
  const resumableSessions = sessions
    ? [
        record("s1", "登录页窄屏时按钮换行，帮我看看 flex 布局", 12),
        record("s2", "Add retry with backoff to the payment webhook", 180),
        record("s3", "Why does the cart total round twice?", 1500),
      ]
    : [];
  return [
    { kind: "theme", vars: theme },
    { kind: "hello", backend: "legacy", projectDir: cwd, permissionModes: ["manual", "auto", "bypass"],
      resumableSessions, expectedVerdandiRevision: null, account: "me@example.com",
      restore: sessions ? { labels: ["api", "docs", "tests"], bypass: 0 } : null },
    // The window's keys as Rust sends them: the start screen's box names the key that moves into the panel.
    { kind: "keymap", prefix: "Ctrl+b", window: [{ keys: "Ctrl+h / Ctrl+l", what: "Module left / right" }],
      prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" },
    { kind: "tabs", active: 1, tabs: [tab] },
    { kind: "pane_focus", focused: true },
  ];
}
