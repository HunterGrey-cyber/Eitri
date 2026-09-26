/** Jump button at the list's bottom edge, reachable by click or HINT; `G` jumps too. */
export function NewPill({ label, onJump }: { label: string; onJump: () => void }) {
  return (
    <button type="button" className="new-pill" onClick={onJump} title="G">
      {label}
    </button>
  );
}
