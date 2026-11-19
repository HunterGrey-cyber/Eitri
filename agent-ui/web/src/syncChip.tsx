import { revealedText } from "./revealHidden";

export const STOPPED_BEFORE_SEND = "stopped before it was sent";
/** A send refused because its session ended while it waited for the sync: the host puts its text back in the tab's
 *  own draft and sends it as `queue_taken`, so the page does not put it back itself. */
export const SESSION_ENDED_BEFORE_SEND = "not sent: the session ended";

export type SendSync = {
  waiting: boolean;
  stopped?: boolean;
  restored?: string[];
  lines: { messageId: string; seq?: number; prompt: string; text: string }[];
};

export type SyncStatus = {
  host: string;
  phase: string;
  gitPct: number;
  conflicts: number;
  pausedReason: string | null;
};

export function SyncChip({ chip }: { chip: SyncStatus }) {
  const paused = chip.phase === "paused" || chip.pausedReason != null;
  const conflict = !paused && chip.conflicts > 0;
  let detail = "";
  if (paused) detail = "paused";
  else if (conflict) detail = `${chip.conflicts} conflicts`;
  else if (chip.phase === "history") detail = `history arriving: ${chip.gitPct} %`;
  else if (chip.phase !== "watching") detail = chip.phase === "recreating" ? "syncing" : chip.phase;
  return (
    <span className="band-seg mode-pill" style={conflict ? { color: "var(--nv-error)" } : undefined}
      title={chip.pausedReason ? revealedText(chip.pausedReason) : undefined}>
      ⇅ {revealedText(chip.host)}{detail && ` · ${revealedText(detail)}`}
    </span>
  );
}
