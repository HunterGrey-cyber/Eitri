import { revealedText } from "./revealHidden";

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
  else if (chip.phase === "history") detail = `history ${chip.gitPct} %`;
  else if (chip.phase !== "watching") detail = chip.phase === "recreating" ? "syncing" : chip.phase;
  return (
    <span className="band-seg mode-pill" style={conflict ? { color: "var(--nv-error)" } : undefined}
      title={chip.pausedReason ? revealedText(chip.pausedReason) : undefined}>
      ⇅ {revealedText(chip.host)}{detail && ` · ${revealedText(detail)}`}
    </span>
  );
}
