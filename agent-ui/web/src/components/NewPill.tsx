/** R2's marker at the list's bottom edge. A button: a click and HINT both reach it; `G` is the key. */
export function NewPill({ label, onJump }: { label: string; onJump: () => void }) {
  return (
    <button type="button" className="new-pill" onClick={onJump} title="G">
      {label}
    </button>
  );
}
