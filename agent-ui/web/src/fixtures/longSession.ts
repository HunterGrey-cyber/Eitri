import { initialState } from "../reducer";
import type { AgentUiState, ThinkingBlock, ToolCallRecord, TranscriptMessage, TurnTimeWire, UserPromptRecord } from "../types";

/* A conversation of a chosen length for the long-session measurements: what a tab kept open for days holds.
   One cycle is a turn of eleven drawn rows: a prompt, a thought, replies with code fences, a list and a table, tool
   calls alone and in a folded run, an edit with its diff, a failed command. Every text differs from every other
   (numbers in it change), so no cache answers for a row a real session would have had to draw. Deterministic: the
   same length gives the same conversation. */

export const ROWS_PER_CYCLE = 11;

const rustFence = (n: number) => {
  const lines = [`pub fn step_${n}(input: &str) -> Result<usize, String> {`];
  for (let i = 0; i < 14; i++) lines.push(`    let value_${i} = input.len() + ${n * (i + 1)}; // keeps the running total`);
  lines.push("    if value_0 == 0 {", `        return Err(format!("nothing at {}", ${n}));`, "    }", "    Ok(value_0)", "}");
  return "```rust\n" + lines.join("\n") + "\n```";
};

const tsFence = (n: number) =>
  "```ts\n" +
  `export function tally${n}(rows: readonly { seq: number; text: string }[]): number {\n` +
  "  let total = 0;\n" +
  `  for (const row of rows) total += row.text.length + ${n};\n` +
  "  return total;\n}\n```";

const longTail = (n: number) =>
  [
    `Done with step **${n}**. The change keeps \`step_${n}\` where it was and moves the check in front of the loop, so a`,
    "row that cannot be parsed never reaches the writer. What I looked at:",
    "",
    `1. \`src/module_${n % 40}.rs\` holds the only caller, and it already handles the error.`,
    `2. The test at line ${100 + n} covered the old order, so it now asserts the new one.`,
    "3. Nothing else reads the file while the writer holds it.",
    "",
    rustFence(n + 1),
    "",
    "The suite passes. One thing I did not change: the retry count is still hard-coded, and it would read better as a",
    `setting. If you want that as well, it is a small change in \`src/settings_${n % 7}.rs\`.`,
  ].join("\n");

const table = (n: number) =>
  ["| file | lines | state |", "|---|---:|---|", ...[0, 1, 2, 3].map((i) => `| \`src/part_${n}_${i}.rs\` | ${n + i * 13} | ${i % 2 ? "changed" : "read"} |`)].join("\n");

const output = (n: number, lines: number) =>
  Array.from({ length: lines }, (_, i) => `test module_${n % 40}::case_${i} ... ok (${(n * (i + 1)) % 997} us)`).join("\n");

const fileBody = (n: number, lines: number) =>
  Array.from({ length: lines }, (_, i) => `${i + 1}\tfn helper_${n}_${i}(value: usize) -> usize { value + ${i} }`).join("\n");

/** Every finished turn's time, as a snapshot sends them: each stands under the turn's last reply, drawn as "Worked for". */
export function longSessionTurnTimes(state: AgentUiState): TurnTimeWire[] {
  const repliesPerTurn = 5;
  return state.transcript.flatMap((message, i) =>
    i % repliesPerTurn === repliesPerTurn - 1
      ? [{ turnId: `turn-${(i + 1) / repliesPerTurn - 1}`, startedAtMs: 1_000 * i, endedAtMs: 1_000 * i + 40_000 + i, anchorSeq: message.seq }]
      : [],
  );
}

/** The conversation, as the state a snapshot carries, of about `rows` drawn rows (in whole cycles). */
export function longSessionState(rows: number): AgentUiState {
  const userPrompts: UserPromptRecord[] = [];
  const transcript: TranscriptMessage[] = [];
  const toolCalls: ToolCallRecord[] = [];
  const thinking: ThinkingBlock[] = [];
  let seq = 1;
  const cycles = Math.ceil(rows / ROWS_PER_CYCLE);
  for (let n = 0; n < cycles; n++) {
    const turnId = `turn-${n}`;
    const reply = (text: string) => transcript.push({ seq: seq++, text });
    const tool = (name: string, input: unknown, content: unknown, isError = false) =>
      toolCalls.push({ seq, toolUseId: `use-${seq++}`, name, input, result: { content, isError }, turnId });
    userPrompts.push({ seq: seq++, text: `Please look at step ${n} and tell me why the writer stops after the first row of module ${n % 40}.` });
    thinking.push({ seq: seq++, turnId, text: `The writer stops at row ${n}.\nThe loop reads until the first error.\nCheck the caller first.` });
    reply(`I will start with the caller of \`step_${n}\`, since the loop itself looks right.`);
    tool("Bash", { command: `cargo test -p module_${n % 40} step_${n}`, description: "Run the step's tests" }, output(n, 24));
    reply(`The failing case is here:\n\n${rustFence(n)}\n\nThe early return skips the writer.`);
    // Two calls in a row with no reply between them fold into one drawn row, as they do in a real turn.
    tool("Read", { file_path: `/work/project/src/module_${n % 40}.rs` }, fileBody(n, 60));
    tool("Grep", { pattern: `step_${n}`, path: "/work/project/src" }, `src/module_${n % 40}.rs:12:step_${n}(line)\nsrc/writer.rs:88:step_${n}(row)`);
    tool("Read", { file_path: `/work/project/src/writer_${n % 9}.rs` }, fileBody(n + 1, 40));
    reply(`Three things follow from that:\n\n- the loop is fine\n- \`step_${n}\` returns early\n- the writer never sees row ${n + 1}\n\n${tsFence(n)}`);
    tool(
      "Edit",
      {
        file_path: `/work/project/src/module_${n % 40}.rs`,
        old_string: `    let total = rows.len() + ${n};\n    if total == 0 {\n        return Ok(0);\n    }\n`,
        new_string: `    let total = rows.len() + ${n + 1};\n    if total == 0 {\n        return Ok(0);\n    }\n    writer.flush()?;\n`,
      },
      `The file /work/project/src/module_${n % 40}.rs has been updated.`,
    );
    reply(`Before and after:\n\n${table(n)}`);
    tool("Bash", { command: `cargo clippy -p module_${n % 40}`, description: "Lint the module" }, `error: unused variable \`value_${n}\`\n${output(n + 1, 6)}`, true);
    reply(longTail(n));
  }
  return {
    ...initialState(),
    backend: "sidecar",
    sessionId: "long-session",
    providerSessionId: "long-session-provider",
    model: "scripted",
    cwd: "/work/project",
    status: { kind: "running" },
    userPrompts,
    transcript,
    toolCalls,
    thinking,
    nextSeq: seq,
  };
}
