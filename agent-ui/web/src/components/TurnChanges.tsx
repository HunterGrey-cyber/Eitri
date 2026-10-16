import type { AgentUiState } from "../types";
import { fileChangesOf } from "../diff";

export type TurnChangeSummary = { files: number; added: number; removed: number };

/** The latest turn: the running one, or else the turn of the newest thing the agent wrote after the
 *  latest prompt that opened a turn. `null` when nothing after that prompt names a turn (a tool-free
 *  turn, or one not started yet), so an older turn's changes are never shown as the latest's.
 *
 *  Read from the conversation itself rather than from the turn times (which hold only turns Rust timed
 *  in this process, not a resumed history) or from the last tool call (which is an older turn's after a
 *  turn that called nothing): a prompt that opened a turn starts the latest one, `seq` is the one order
 *  shared with Rust, and every call, thought and ending carries its turn. A message read inside the
 *  running turn (`delivered`) opens none. */
export function latestTurnId(state: AgentUiState): string | null {
  if (state.activeTurnId !== null) return state.activeTurnId;
  let opened = -1;
  for (const prompt of state.userPrompts) if (!prompt.delivered && prompt.seq > opened) opened = prompt.seq;
  let newestSeq = opened;
  let turn: string | null = null;
  for (const { seq, turnId } of [...state.toolCalls, ...state.thinking, ...state.turnEndings]) {
    if (seq > newestSeq && turnId) {
      newestSeq = seq;
      turn = turnId;
    }
  }
  return turn;
}

/** What the latest turn (`latestTurnId`) changed on disk, from its finished file-changing calls (Claude's
 *  edits, Codex's `fileChange`, an ACP agent's edit, delete and move): how many files and how many lines
 *  each way. `null` when it changed nothing, and for a turn that called nothing. */
export function latestTurnChanges(state: AgentUiState): TurnChangeSummary | null {
  const turn = latestTurnId(state);
  if (turn === null) return null;
  const files = new Set<string>();
  let added = 0;
  let removed = 0;
  for (const call of state.toolCalls) {
    if (call.turnId !== turn || call.result === null || call.result.isError) continue;
    for (const change of fileChangesOf(call.name, call.input)) {
      files.add(change.filePath);
      added += change.added;
      removed += change.removed;
    }
  }
  return files.size === 0 ? null : { files: files.size, added, removed };
}

/** One quiet line above the composer: the latest turn's changes, and once the turn is over, the key
 *  that opens them for review. `c` reviews the tab's latest turn, the same turn counted here, so the key
 *  is offered only beside that turn's own count. */
export function TurnChanges({ state }: { state: AgentUiState }) {
  const changes = latestTurnChanges(state);
  if (changes === null) return null;
  return (
    <div className="turn-changes" aria-label="Changes in the latest turn">
      <span>{changes.files === 1 ? "1 file changed" : `${changes.files} files changed`}</span>
      <span className="edit-count-added">
        <span className="edit-sign">+</span>
        {changes.added}
      </span>
      <span className="edit-count-removed">
        <span className="edit-sign">−</span>
        {changes.removed}
      </span>
      {state.activeTurnId === null && <span className="turn-changes-key">c to review</span>}
    </div>
  );
}
